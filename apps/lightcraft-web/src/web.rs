//! Browser host: opens the library in browser storage, starts the render workers and eframe on
//! `<canvas id="lightcraft_canvas">`, wires the file picker and drag-and-drop, and turns exports
//! into downloads.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use lightcraft_engine::Session;
use lightcraft_engine::catalog::Source;
use lightcraft_engine::library::LibraryStores;
use lightcraft_ui_egui::{LightcraftApp, Services, UiState};
use serde_json::json;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;

use crate::backend::Backend;
use crate::bench::Bench;
use crate::files::{Files, FlushOp, LIBRARY_FILES};
use crate::store::{Originals, content_hash, download_name, storage_key};
use crate::wire::ThumbIndex;
use crate::workers::{THUMB_INDEX, Workers};

const ACCEPT: &str = ".jpg,.jpeg,.png,.tif,.tiff,.webp,.dng,.cr2,.nef,.arw,.psd,.jxl,.gif,.bmp";

/// Where the library files live in browser storage.
const LIBRARY_DIR: &str = "library";

/// How often view state, UI prefs and the thumbnail index are saved (ms).
const SAVE_EVERY_MS: f64 = 1000.0;

/// The page's window (`None` only outside a browser page, e.g. in a worker).
fn window() -> Option<web_sys::Window> {
    web_sys::window()
}

/// Milliseconds since navigation start.
fn perf_now() -> f64 {
    window().and_then(|w| w.performance()).map(|p| p.now()).unwrap_or(0.0)
}

fn set_status(text: &str) {
    if let Some(el) = window().and_then(|w| w.document()).and_then(|d| d.get_element_by_id("lightcraft_status")) {
        el.set_text_content(Some(text));
    }
}

fn library_key(name: &str) -> String {
    format!("{LIBRARY_DIR}/{name}")
}

/// URL options (`?bench&store=idb&workers=2&reset`).
struct Options {
    bench: bool,
    /// "opfs" (default: OPFS, falling back to IndexedDB), "idb" or "memory".
    store: String,
    workers: Option<usize>,
    reset: bool,
}

impl Options {
    fn from_url() -> Options {
        let q = window().and_then(|w| w.location().search().ok()).unwrap_or_default();
        let params: Vec<(String, String)> = q
            .trim_start_matches('?')
            .split('&')
            .filter(|p| !p.is_empty())
            .map(|p| match p.split_once('=') {
                Some((k, v)) => (k.to_string(), v.to_string()),
                None => (p.to_string(), String::new()),
            })
            .collect();
        let get = |k: &str| params.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        Options {
            bench: get("bench").is_some(),
            store: get("store").filter(|s| s == "idb" || s == "memory").unwrap_or_else(|| "opfs".into()),
            workers: get("workers").and_then(|v| v.parse().ok()),
            reset: get("reset").is_some(),
        }
    }
}

/// Store a picked/dropped file (bytes into browser storage, then queue its import).
async fn store_file(originals: Originals, backend: Option<Backend>, name: String, bytes: Vec<u8>, ctx: egui::Context) {
    let hash = content_hash(&bytes);
    if let Some(b) = &backend
        && let Err(e) = b.write(&storage_key(&hash), &bytes).await
    {
        // still importable for this session; it won't survive a reload
        log::error!("storing {name} in browser storage failed: {e}");
    }
    originals.added(&name, &hash, Arc::from(bytes));
    ctx.request_repaint();
}

/// Read a browser `File` into storage.
async fn read_file(originals: Originals, backend: Option<Backend>, file: web_sys::File, ctx: egui::Context) {
    match wasm_bindgen_futures::JsFuture::from(file.array_buffer()).await {
        Ok(buf) => store_file(originals, backend, file.name(), js_sys::Uint8Array::new(&buf).to_vec(), ctx).await,
        Err(e) => log::warn!("reading {}: {e:?}", file.name()),
    }
}

/// Show the browser's open dialog; picked files are imported asynchronously.
fn open_picker(originals: Originals, backend: Option<Backend>, ctx: egui::Context) {
    let Some(doc) = window().and_then(|w| w.document()) else { return };
    let Ok(input) = doc.create_element("input").map(|e| e.unchecked_into::<web_sys::HtmlInputElement>()) else { return };
    input.set_type("file");
    input.set_multiple(true);
    input.set_accept(ACCEPT);
    let inp = input.clone();
    let on_change = Closure::<dyn FnMut()>::new(move || {
        if let Some(files) = inp.files() {
            for i in 0..files.length() {
                if let Some(f) = files.get(i) {
                    wasm_bindgen_futures::spawn_local(read_file(originals.clone(), backend.clone(), f, ctx.clone()));
                }
            }
        }
    });
    input.set_onchange(Some(on_change.as_ref().unchecked_ref()));
    on_change.forget();
    input.click();
}

/// Offer `bytes` as a download named after `path`.
fn download(path: &str, bytes: &[u8]) -> Result<(), String> {
    let e = |e: JsValue| format!("download failed: {e:?}");
    let arr = js_sys::Array::of1(&js_sys::Uint8Array::from(bytes));
    let opts = web_sys::BlobPropertyBag::new();
    let name = download_name(path);
    opts.set_type(crate::store::mime_for(name));
    let blob = web_sys::Blob::new_with_u8_array_sequence_and_options(&arr, &opts).map_err(e)?;
    let url = web_sys::Url::create_object_url_with_blob(&blob).map_err(e)?;
    let doc = window().and_then(|w| w.document()).ok_or("no document")?;
    let a: web_sys::HtmlAnchorElement = doc.create_element("a").map_err(e)?.unchecked_into();
    a.set_href(&url);
    a.set_download(name);
    a.click();
    // revoke once the click has been handled
    let revoke = Closure::once_into_js(move || {
        let _ = web_sys::Url::revoke_object_url(&url);
    });
    if let Some(w) = window() {
        let _ = w.set_timeout_with_callback_and_timeout_and_arguments_0(revoke.unchecked_ref(), 5000);
    }
    log::info!("exported {name} ({} bytes)", bytes.len());
    Ok(())
}

fn services(originals: Originals, backend: Option<Backend>, ctx: egui::Context) -> Services {
    Services {
        pick_files: Some(Box::new(move || {
            open_picker(originals.clone(), backend.clone(), ctx.clone());
            Vec::new() // files arrive asynchronously and are imported on a later frame
        })),
        // Preset files: browser pickers are asynchronous; not wired on the web yet.
        pick_preset_files: None,
        pick_tracklog: None,
        save_preset_file: None,
        pick_curve_preset_files: None,
        save_curve_preset_file: None,
        write: Some(Box::new(download)),
        // downloads happen on the main thread: exports run in the foreground on the web
        write_shared: None,
        png: Some(Box::new(|img: &lightcraft_raster::Rgba8| {
            lightcraft_codecs::encode_png(&lightcraft_codecs::EncodeImage::rgba8(img), &lightcraft_codecs::EncodeMeta::default()).unwrap_or_default()
        })),
        reveal: None,
        open_with: None,
        open_url: Some(Box::new(|url: &str| {
            let w = web_sys::window().ok_or("no window")?;
            w.open_with_url_and_target(url, "_blank").map(|_| ()).map_err(|_| "the browser blocked the new tab".to_string())
        })),
        pick_folder: None,
    }
}

/// Write dirty library files to storage, in order, until none are left.
async fn flush(files: Files, backend: Backend, flushing: Rc<Cell<bool>>) {
    'outer: loop {
        let ops = files.take_dirty();
        if ops.is_empty() {
            break;
        }
        for (i, op) in ops.iter().enumerate() {
            let r = match op {
                FlushOp::Write { name, data } => backend.write(&library_key(name), data).await,
                FlushOp::Append { name, offset, data } => backend.write_at(&library_key(name), Some(*offset), data).await,
            };
            if let Err(e) = r {
                log::error!("saving {} failed: {e}", op.name());
                files.failed(&ops[i..]);
                break 'outer;
            }
        }
    }
    flushing.set(false);
}

/// Everything loaded before the UI starts.
struct Boot {
    opts: Options,
    backend: Option<Backend>,
    files: Files,
    index: ThumbIndex,
}

async fn boot(opts: Options) -> Boot {
    let t0 = perf_now();
    let backend = if opts.store == "memory" {
        None
    } else {
        match Backend::open(opts.store == "idb").await {
            Ok(b) => Some(b),
            Err(e) => {
                log::error!("no browser storage ({e}); this session won't be saved");
                None
            }
        }
    };
    let files = Files::default();
    let mut index = ThumbIndex::default();
    if let Some(b) = &backend {
        if opts.reset {
            match b.clear().await {
                Ok(()) => log::info!("lightcraft: storage cleared (?reset)"),
                Err(e) => log::error!("clearing storage: {e}"),
            }
        }
        for name in LIBRARY_FILES {
            match b.read(&library_key(name)).await {
                Ok(Some(data)) => files.preload(name, data),
                Ok(None) => {}
                Err(e) => log::error!("reading {name}: {e}"),
            }
        }
        if let Ok(Some(j)) = b.read(THUMB_INDEX).await {
            index = ThumbIndex::from_json(&j);
        }
        crate::backend::request_persistence();
        // thumbnails stored without an index entry (index saved late, or lost): delete them
        let b2 = b.clone();
        let known: std::collections::HashSet<String> = b
            .list("thumbs")
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|n| n.ends_with(".jpg"))
            .map(|n| n.trim_end_matches(".jpg").to_string())
            .collect();
        let orphans: Vec<String> = known.into_iter().filter(|k| !index.contains(k)).collect();
        if !orphans.is_empty() {
            wasm_bindgen_futures::spawn_local(async move {
                for k in &orphans {
                    let _ = b2.remove(&crate::wire::thumb_storage_key(k)).await;
                }
                log::info!("thumbnail cache: removed {} unindexed files", orphans.len());
            });
        }
    }
    log::info!(
        "lightcraft: storage {} opened in {:.0} ms ({} thumbnails indexed, {:.1} MB)",
        backend.as_ref().map_or("memory", |b| b.kind()),
        perf_now() - t0,
        index.len(),
        index.total() as f64 / 1e6
    );
    Boot { opts, backend, files, index }
}

struct WebApp {
    app: LightcraftApp,
    originals: Originals,
    files: Files,
    backend: Option<Backend>,
    workers: Option<Workers>,
    flushing: Rc<Cell<bool>>,
    bench: Option<Bench>,
    first_frame_logged: bool,
    last_save: f64,
    ui_written: Vec<u8>,
}

impl WebApp {
    fn new(cc: &eframe::CreationContext<'_>, boot: Boot) -> Self {
        let Boot { opts, backend, files, index } = boot;
        let originals = Originals::default();
        let t = perf_now();
        let mut session = Session::new();
        originals.install(&mut session);
        let stores = LibraryStores {
            dir: format!("browser:{}/{LIBRARY_DIR}", backend.as_ref().map_or("memory", |b| b.kind())).into(),
            catalog: Box::new(files.store()),
            files: Box::new(files.store()),
            on_disk: false,
        };
        match session.open_library_in(stores, true).cloned() {
            Ok(r) => log::info!(
                "lightcraft: library {} in {:.0} ms ({} photos; snapshot seq {}, {} ops replayed)",
                if r.created { "created" } else { "loaded" },
                perf_now() - t,
                session.catalog.len(),
                r.snapshot_seq,
                r.replayed
            ),
            Err(e) => {
                log::error!("opening the library failed: {e}; starting a temporary one");
                session = Session::with_demo();
                originals.install(&mut session);
            }
        }
        // previews are large (≤ 2560 px, f32): keep few in a 32-bit address space
        session.media.preview_capacity = 3;
        let mut app = LightcraftApp::new(session, services(originals.clone(), backend.clone(), cc.egui_ctx.clone()));
        let ui_written = files.get("ui.json").unwrap_or_default();
        if let Ok(ui) = serde_json::from_slice::<UiState>(&ui_written) {
            app.ui = ui;
        }
        app.ui = app.ui.sanitized();
        let n = opts.workers.unwrap_or_else(|| {
            let cores = window().map_or(1, |w| w.navigator().hardware_concurrency() as usize);
            cores.saturating_sub(1).clamp(1, 4)
        });
        let workers =
            (n > 0).then(|| Workers::start(n, backend.as_ref().map_or("memory", |b| b.kind()), backend.clone(), index, cc.egui_ctx.clone()));
        if let Some(w) = &workers {
            app.renderer.set_offload(Box::new(w.clone()));
        }
        log::info!("lightcraft: {n} render workers");
        CTX.with(|c| *c.borrow_mut() = Some(cc.egui_ctx.clone()));
        let origin = lightcraft_ui_egui::now_ms() - perf_now();
        WebApp {
            app,
            originals,
            files,
            backend,
            workers,
            flushing: Rc::new(Cell::new(false)),
            bench: opts.bench.then(|| Bench::new(origin)),
            first_frame_logged: false,
            last_save: 0.0,
            ui_written,
        }
    }

    fn import_dropped(&mut self, ctx: &egui::Context) {
        let dropped = ctx.input(|i| i.raw.dropped_files.clone());
        for f in dropped {
            let (originals, backend, ctx) = (self.originals.clone(), self.backend.clone(), ctx.clone());
            wasm_bindgen_futures::spawn_local(async move {
                let name = f.path().to_string_lossy().to_string();
                match f.bytes_async().await {
                    Ok(bytes) => store_file(originals, backend, name, bytes, ctx).await,
                    Err(e) => log::warn!("reading dropped {name}: {e}"),
                }
            });
        }
        let paths = self.originals.take_pending();
        if !paths.is_empty() {
            let n = paths.len();
            match self.app.run("library.import", json!({"paths": paths})) {
                Ok(_) => self.app.toast(ctx, format!("Added {n} photo{}", if n == 1 { "" } else { "s" })),
                Err(e) => log::warn!("import: {e}"),
            }
        }
    }

    /// Load originals the main thread asked for (and the active photo's, ahead of time).
    fn load_originals(&mut self, ctx: &egui::Context) {
        if let Some(id) = self.app.session.selection.active
            && let Some(Source::File { path }) = self.app.session.catalog.photo(id).map(|p| &p.source)
            && !self.originals.contains(path)
        {
            self.originals.prefetch(path);
        }
        let Some(b) = &self.backend else { return };
        for hash in self.originals.take_wanted() {
            let (b, originals, ctx) = (b.clone(), self.originals.clone(), ctx.clone());
            wasm_bindgen_futures::spawn_local(async move {
                match b.read(&storage_key(&hash)).await {
                    Ok(Some(bytes)) => {
                        originals.insert(&hash, Arc::from(bytes));
                        ctx.request_repaint();
                    }
                    Ok(None) => {
                        log::warn!("original {hash} is not in browser storage");
                        originals.load_failed(&hash);
                    }
                    Err(e) => {
                        log::warn!("loading original {hash}: {e}");
                        originals.load_failed(&hash);
                    }
                }
            });
        }
    }

    /// Save view state, UI prefs and the thumbnail index now and then; flush dirty files.
    fn save(&mut self) {
        let now = perf_now();
        if now - self.last_save >= SAVE_EVERY_MS {
            self.last_save = now;
            self.app.session.save_view();
            if let Ok(ui) = serde_json::to_vec_pretty(&self.app.ui)
                && ui != self.ui_written
            {
                self.files.write("ui.json", &ui);
                self.ui_written = ui;
            }
            if let (Some(w), Some(b)) = (&self.workers, &self.backend)
                && let Some(index) = w.take_index_if_dirty()
            {
                let b = b.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    if let Err(e) = b.write(THUMB_INDEX, &index).await {
                        log::warn!("saving the thumbnail index: {e}");
                    }
                });
            }
        }
        if let Some(b) = &self.backend
            && self.files.is_dirty()
            && !self.flushing.get()
        {
            self.flushing.set(true);
            wasm_bindgen_futures::spawn_local(flush(self.files.clone(), b.clone(), self.flushing.clone()));
        }
    }
}

type Reply = (js_sys::Function, js_sys::Function);

thread_local! {
    /// Commands from JavaScript ([`command`]), run on the next frame.
    static INBOX: std::cell::RefCell<Vec<(String, String, Reply)>> = const { std::cell::RefCell::new(Vec::new()) };
    static CTX: std::cell::RefCell<Option<egui::Context>> = const { std::cell::RefCell::new(None) };
}

/// Run a command by id from JavaScript (automation, tests): `await command("library.info", "{}")`
/// resolves to the result as JSON text, or rejects with the error. Besides every engine/UI
/// command, `web.stats` reports the browser host's state (storage, workers, originals in memory).
#[wasm_bindgen]
pub fn command(id: String, params: String) -> js_sys::Promise {
    js_sys::Promise::new(&mut |resolve, reject| {
        INBOX.with(|q| q.borrow_mut().push((id.clone(), params.clone(), (resolve, reject))));
        CTX.with(|c| c.borrow().as_ref().map(|c| c.request_repaint()));
    })
}

impl WebApp {
    fn web_stats(&self) -> serde_json::Value {
        let (alive, ready, remote, inline) = self.workers.as_ref().map_or((0, 0, 0, 0), Workers::stats);
        let (orig_n, orig_bytes) = self.originals.usage();
        json!({
            "storage": self.backend.as_ref().map_or("memory", |b| b.kind()),
            "dirty": self.files.is_dirty(),
            "flushing": self.flushing.get(),
            "workers": {"alive": alive, "ready": ready, "remoteJobs": remote, "inlineJobs": inline},
            "originalsInMemory": orig_n,
            "originalBytesInMemory": orig_bytes,
            "renderQueue": self.app.renderer.queued(),
            "rendersInFlight": self.app.renderer.in_flight(),
            "thumbTextures": self.app.renderer.thumb_textures(),
        })
    }

    fn run_inbox(&mut self) {
        let inbox = INBOX.with(|q| std::mem::take(&mut *q.borrow_mut()));
        for (id, params, (resolve, reject)) in inbox {
            let params: serde_json::Value = serde_json::from_str(&params).unwrap_or(json!({}));
            let r = if id == "web.stats" { Ok(self.web_stats()) } else { self.app.run(&id, params) };
            let _ = match r {
                Ok(v) => resolve.call1(&JsValue::NULL, &v.to_string().into()),
                Err(e) => reject.call1(&JsValue::NULL, &e.into()),
            };
        }
    }
}

impl eframe::App for WebApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.run_inbox();
        self.import_dropped(ctx);
        self.load_originals(ctx);
        self.app.logic(ctx);
        self.save();
        if let Some(b) = self.bench.as_mut()
            && let Some(report) = b.step(&mut self.app)
        {
            log::info!("lightcraft-bench {report}");
            if let Some((alive, ready, remote, inline)) = self.workers.as_ref().map(Workers::stats) {
                log::info!("lightcraft-workers {{\"alive\":{alive},\"ready\":{ready},\"remote_jobs\":{remote},\"inline_jobs\":{inline}}}");
            }
            set_status(&format!("bench: {report}"));
            self.bench = None;
        }
        if self.bench.is_some() {
            ctx.request_repaint();
        }
    }

    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw: &mut egui::RawInput) {
        self.app.raw_input_hook(raw);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.app.ui(ui);
        if !self.first_frame_logged && !self.app.widgets.is_empty() {
            self.first_frame_logged = true;
            log::info!("lightcraft first UI frame at {:.0} ms after navigation start", perf_now());
        }
    }
}

/// Start the app (called by `index.html` after instantiating the module).
#[wasm_bindgen]
pub fn start() {
    eframe::WebLogger::init(log::LevelFilter::Info).ok();
    log::info!("lightcraft: wasm instantiated at {:.0} ms", perf_now());
    let opts = Options::from_url();
    wasm_bindgen_futures::spawn_local(async move {
        let Some(canvas) = window()
            .and_then(|w| w.document())
            .and_then(|d| d.get_element_by_id("lightcraft_canvas"))
            .and_then(|e| e.dyn_into::<web_sys::HtmlCanvasElement>().ok())
        else {
            log::error!("missing <canvas id=\"lightcraft_canvas\">");
            return;
        };
        let boot = boot(opts).await;
        let runner = eframe::WebRunner::new();
        let r = runner.start(canvas, eframe::WebOptions::default(), Box::new(move |cc| Ok(Box::new(WebApp::new(cc, boot))))).await;
        match r {
            Ok(()) => {
                if let Some(el) = window().and_then(|w| w.document()).and_then(|d| d.get_element_by_id("lightcraft_loading")) {
                    el.remove();
                }
            }
            Err(e) => {
                log::error!("LightCraft failed to start: {e:?}");
                set_status(&format!("LightCraft failed to start: {e:?}"));
            }
        }
    });
}
