//! Cached application icons. Desktop lookup, filesystem access and image decoding
//! run on a worker; only pixel-buffer imports happen on the compositor thread.
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};

use gio::prelude::*;
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use smithay::utils::Transform;

pub const ICON_SIZE: i32 = 48;
type Key = (String, i32);
type Reply = (Key, Option<Vec<u8>>);

#[derive(Debug, Default)]
pub struct AppIconCache(RefCell<Cache>);

#[derive(Debug, Default)]
struct Cache {
    entries: HashMap<Key, Entry>,
    worker: Option<Worker>,
}

#[derive(Debug)]
struct Entry {
    buffer: MemoryRenderBuffer,
    pending: bool,
}

#[derive(Debug)]
struct Worker {
    requests: Sender<Key>,
    replies: Receiver<Reply>,
}

impl AppIconCache {
    pub fn is_loading(&self) -> bool {
        let mut cache = self.0.borrow_mut();
        let pending = cache.entries.values().any(|entry| entry.pending);
        cache.receive();
        // Keep one final redraw when the worker has just supplied the real icon.
        pending
    }

    pub fn get(&self, app_id: Option<&str>, scale: f64) -> MemoryRenderBuffer {
        // Oversample fractional outputs while retaining exactly 48 logical pixels.
        let scale = scale.ceil().clamp(1., 8.) as i32;
        let key = (app_id.unwrap_or_default().to_owned(), scale);
        let mut cache = self.0.borrow_mut();
        cache.receive();
        if let Some(entry) = cache.entries.get(&key) { return entry.buffer.clone(); }
        if cache.entries.len() >= 256 {
            // Bound GPU and pixel memory; pending requests still get their reply drained.
            cache.entries.retain(|_, entry| entry.pending);
            if cache.entries.len() >= 256 { return buffer(&fallback(ICON_SIZE * scale), scale); }
        }
        if cache.worker.is_none() {
            let (requests, incoming) = mpsc::channel::<Key>();
            let (outgoing, replies) = mpsc::channel::<Reply>();
            if std::thread::Builder::new().name("niri-app-icons".into()).spawn(move || {
                let resolver = Resolver::new();
                for key in incoming {
                    let pixels = resolver.load(&key.0, ICON_SIZE * key.1);
                    if outgoing.send((key, pixels)).is_err() { break; }
                }
            }).is_ok() {
                cache.worker = Some(Worker { requests, replies });
            }
        }
        let pending = cache.worker.as_ref().is_some_and(|worker| worker.requests.send(key.clone()).is_ok());
        let buffer = buffer(&fallback(ICON_SIZE * scale), scale);
        cache.entries.insert(key, Entry { buffer: buffer.clone(), pending });
        buffer
    }
}

impl Cache {
    fn receive(&mut self) {
        let Some(worker) = &self.worker else { return; };
        loop {
            match worker.replies.try_recv() {
                Ok((key, pixels)) => {
                    if let Some(entry) = self.entries.get_mut(&key) {
                        if let Some(pixels) = pixels { entry.buffer = buffer(&pixels, key.1); }
                        entry.pending = false;
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    for entry in self.entries.values_mut() { entry.pending = false; }
                    break;
                }
            }
        }
    }
}

fn buffer(pixels: &[u8], scale: i32) -> MemoryRenderBuffer {
    MemoryRenderBuffer::from_slice(pixels, Fourcc::Abgr8888,
        (ICON_SIZE * scale, ICON_SIZE * scale), scale, Transform::Normal, None)
}

fn fallback(size: i32) -> Vec<u8> {
    let mut pixels = vec![0; (size * size * 4) as usize];
    for y in 0..size {
        for x in 0..size {
            // The fallback artwork uses a 24-unit grid, independent of display size.
            let (lx, ly) = (x * 24 / size, y * 24 / size);
            if !(2..22).contains(&lx) || !(3..21).contains(&ly) { continue; }
            let color = if ly < 7 { [99, 102, 241, 255] }
                else if lx == 2 || lx == 21 || ly == 20 { [160, 164, 184, 255] }
                else { [232, 234, 242, 255] };
            let offset = ((y * size + x) * 4) as usize;
            pixels[offset..offset + 4].copy_from_slice(&color);
        }
    }
    pixels
}

type Ini = HashMap<String, HashMap<String, String>>;

fn ini(path: &Path) -> Ini {
    let mut result = Ini::new();
    let mut section = String::new();
    for line in std::fs::read_to_string(path).unwrap_or_default().lines() {
        let line = line.trim();
        if line.starts_with('#') || line.starts_with(';') { continue; }
        if let Some(name) = line.strip_prefix('[').and_then(|line| line.strip_suffix(']')) {
            section = name.to_owned();
        } else if let Some((key, value)) = line.split_once('=') {
            result.entry(section.clone()).or_default().insert(key.trim().to_owned(), value.trim().trim_matches('"').to_owned());
        }
    }
    result
}

struct Resolver {
    roots: Vec<PathBuf>,
    pixmaps: Vec<PathBuf>,
    theme: String,
}

impl Resolver {
    fn new() -> Self {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        let data_home = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/share"));
        let config = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));
        let data_dirs = std::env::var_os("XDG_DATA_DIRS").unwrap_or_else(|| "/usr/local/share:/usr/share".into());
        let mut dirs = vec![data_home];
        dirs.extend(std::env::split_paths(&data_dirs));
        dirs.push(home.join(".local/share/flatpak/exports/share"));
        dirs.push(PathBuf::from("/var/lib/flatpak/exports/share"));
        let mut roots = vec![home.join(".icons")];
        roots.extend(dirs.iter().map(|dir| dir.join("icons")));
        let theme = [("gtk-3.0/settings.ini", "Settings", "gtk-icon-theme-name"),
            ("gtk-4.0/settings.ini", "Settings", "gtk-icon-theme-name"),
            ("kdeglobals", "Icons", "Theme")].into_iter().find_map(|(file, section, key)| {
                ini(&config.join(file)).get(section)?.get(key).filter(|value| !value.is_empty()).cloned()
            }).unwrap_or_else(|| "hicolor".into());
        Self { roots, pixmaps: dirs.iter().map(|dir| dir.join("pixmaps")).collect(), theme }
    }

    fn load(&self, app_id: &str, size: i32) -> Option<Vec<u8>> {
        let desktop_id = if app_id.ends_with(".desktop") { app_id.to_owned() } else { format!("{app_id}.desktop") };
        let info = gio::DesktopAppInfo::new(&desktop_id).or_else(|| {
            if app_id.is_empty() { return None; }
            gio::AppInfo::all().into_iter().filter_map(|info| info.downcast::<gio::DesktopAppInfo>().ok())
                .find(|info| info.id().is_some_and(|id| id.eq_ignore_ascii_case(&desktop_id))
                    || info.startup_wm_class().is_some_and(|class| class.eq_ignore_ascii_case(app_id)))
        });
        let mut names = Vec::new();
        if let Some(icon) = info.and_then(|info| info.icon()) {
            if let Some(file) = icon.downcast_ref::<gio::FileIcon>() {
                if let Some(path) = file.file().path() {
                    if let Some(pixels) = decode(&path, size) { return Some(pixels); }
                }
            }
            if let Some(icon) = icon.downcast_ref::<gio::ThemedIcon>() {
                names.extend(icon.names().into_iter().map(|name| name.to_string()));
            }
        }
        names.push(app_id.trim_end_matches(".desktop").to_owned());
        names.push(app_id.trim_end_matches(".desktop").to_lowercase());
        names.extend(["application-x-executable".into(), "application-default-icon".into()]);
        for name in names {
            // Icon names are basenames. Absolute file icons are handled separately.
            if name.is_empty() || name.contains('/') || name == ".." { continue; }
            let mut seen = HashSet::new();
            for theme in [&*self.theme, "hicolor", "breeze", "Adwaita"] {
                if let Some(pixels) = self.theme_icon(theme, &name, size, &mut seen) { return Some(pixels); }
            }
            for root in self.roots.iter().chain(&self.pixmaps) {
                if let Some(pixels) = decode_named(root, &name, size) { return Some(pixels); }
            }
        }
        None
    }

    fn theme_icon(&self, theme: &str, name: &str, size: i32, seen: &mut HashSet<String>) -> Option<Vec<u8>> {
        if theme.contains('/') || theme == ".." || !seen.insert(theme.to_owned()) || seen.len() > 32 { return None; }
        let mut candidates = Vec::new();
        let mut inherits = Vec::new();
        for root in &self.roots {
            let root = root.join(theme);
            let index = ini(&root.join("index.theme"));
            let Some(meta) = index.get("Icon Theme") else { continue; };
            if let Some(value) = meta.get("Inherits") {
                inherits.extend(value.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned));
            }
            for directory in ["Directories", "ScaledDirectories"].into_iter()
                .filter_map(|key| meta.get(key)).flat_map(|value| value.split(',')) {
                let directory = directory.trim();
                if Path::new(directory).is_absolute() || Path::new(directory).components()
                    .any(|c| matches!(c, std::path::Component::ParentDir)) { continue; }
                let Some(section) = index.get(directory) else { continue; };
                let number = |key: &str, default| section.get(key).and_then(|n| n.parse::<i32>().ok()).unwrap_or(default);
                let scale = number("Scale", 1).clamp(1, 16);
                let nominal = number("Size", 24).clamp(1, 4096);
                let (min, max) = match section.get("Type").map(String::as_str) {
                    Some("Scalable") => (number("MinSize", nominal), number("MaxSize", nominal)),
                    Some("Fixed") => (nominal, nominal),
                    _ => (nominal - number("Threshold", 2), nominal + number("Threshold", 2)),
                };
                let min = min.clamp(1, 4096) * scale;
                let max = max.clamp(1, 4096).max(min / scale) * scale;
                let distance = if size < min { min - size } else if size > max { size - max } else { 0 };
                candidates.push((distance, root.join(directory)));
            }
        }
        candidates.sort_by_key(|(distance, _)| *distance);
        for (_, directory) in candidates {
            if let Some(pixels) = decode_named(&directory, name, size) { return Some(pixels); }
        }
        for parent in inherits {
            if let Some(pixels) = self.theme_icon(&parent, name, size, seen) { return Some(pixels); }
        }
        None
    }
}

fn decode_named(directory: &Path, name: &str, size: i32) -> Option<Vec<u8>> {
    for extension in ["png", "svg", "xpm"] {
        let file = if name.ends_with(&format!(".{extension}")) { name.to_owned() } else { format!("{name}.{extension}") };
        if let Some(pixels) = decode(&directory.join(file), size) { return Some(pixels); }
    }
    None
}

fn decode(path: &Path, size: i32) -> Option<Vec<u8>> {
    if !path.is_file() { return None; }
    let pixbuf = gdk_pixbuf::Pixbuf::from_file_at_scale(path, size, size, true).ok()?;
    let (width, height) = (pixbuf.width(), pixbuf.height());
    if width <= 0 || height <= 0 || width > size || height > size || pixbuf.bits_per_sample() != 8 { return None; }
    let channels = pixbuf.n_channels() as usize;
    if channels != 3 && channels != 4 { return None; }
    let bytes = pixbuf.read_pixel_bytes();
    let source = bytes.as_ref();
    let stride = pixbuf.rowstride() as usize;
    let mut result = vec![0; (size * size * 4) as usize];
    for y in 0..height as usize {
        for x in 0..width as usize {
            let src = y * stride + x * channels;
            let dst = (((size - height) as usize / 2 + y) * size as usize + (size - width) as usize / 2 + x) * 4;
            let alpha = if channels == 4 { *source.get(src + 3)? } else { 255 };
            for channel in 0..3 {
                result[dst + channel] = ((*source.get(src + channel)? as u16 * alpha as u16 + 127) / 255) as u8;
            }
            result[dst + 3] = alpha;
        }
    }
    Some(result)
}
