//! Real program icons for the result rows.
//!
//! Windows will hand over the icon of any program, Start-menu shortcut, packaged (Store) app or
//! file type through the shell. Asking takes a few milliseconds, so it never happens on the UI
//! thread: rows are drawn immediately with their coloured letter tile, a worker thread fetches the
//! icons (rows on screen now first), and each tile is swapped for the real icon as it arrives.
//! Icons are cached for the session; a failed lookup is remembered so it is not retried.

use eframe::egui;
use okra_core::{Action, Item, Kind};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::mpsc::{channel, Receiver, Sender};

/// Past this many cached icons the cache is dropped and refilled on demand.
const MAX_CACHED: usize = 600;

/// `(cache key, shell parsing name)`: what to remember the icon under, and what to ask Windows for.
pub type IconRequest = (String, String);

type Finished = (String, Option<egui::ColorImage>);

/// Works out which icon a result should show, if any.
pub fn icon_request(item: &Item) -> Option<IconRequest> {
    match (&item.kind, &item.action) {
        (Kind::App, Action::Launch { path, .. }) => {
            let path = path.to_string_lossy().into_owned();
            Some((path.clone(), path))
        }
        (Kind::App, Action::LaunchAppId(id)) => {
            let name = format!("shell:AppsFolder\\{id}");
            Some((name.clone(), name))
        }
        (Kind::File, Action::OpenPath(path)) => {
            let source = path.to_string_lossy().into_owned();
            let ext = path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).unwrap_or_default();
            // Programs and shortcuts each have their own icon; every other file type shares one
            // icon per extension, so a list of 40 PDFs costs a single lookup.
            let per_file = matches!(ext.as_str(), "" | "exe" | "lnk" | "ico" | "url" | "msi" | "scr" | "cpl" | "appref-ms");
            let key = if per_file { source.clone() } else { format!("ext:.{ext}") };
            Some((key, source))
        }
        (Kind::Folder, Action::OpenPath(path)) => Some(("folder".to_string(), path.to_string_lossy().into_owned())),
        _ => None,
    }
}

pub struct Icons {
    /// `None` = looked up and unavailable; keep the letter tile.
    textures: HashMap<String, Option<egui::TextureHandle>>,
    requested: HashSet<String>,
    /// `None` where there is nothing to fetch icons with (non-Windows builds).
    jobs: Option<Sender<Vec<IconRequest>>>,
    done: Receiver<Finished>,
    ctx: egui::Context,
}

impl Icons {
    pub fn new(ctx: &egui::Context) -> Self {
        let (done_tx, done) = channel();
        Self { textures: HashMap::new(), requested: HashSet::new(), jobs: start_worker(ctx, done_tx), done, ctx: ctx.clone() }
    }

    /// Queue any icons not already cached or on their way. Cheap to call for every result list.
    pub fn request<'a>(&mut self, wanted: impl Iterator<Item = &'a IconRequest>) {
        let Some(jobs) = &self.jobs else { return };
        let batch: Vec<IconRequest> = wanted
            .filter(|(key, _)| !self.textures.contains_key(key) && self.requested.insert(key.clone()))
            .cloned()
            .collect();
        if !batch.is_empty() {
            let _ = jobs.send(batch);
        }
    }

    /// Turn finished icons into textures. Call once per frame.
    pub fn poll(&mut self) {
        while let Ok((key, image)) = self.done.try_recv() {
            let texture = image.map(|image| self.ctx.load_texture(format!("icon:{key}"), image, egui::TextureOptions::LINEAR));
            self.textures.insert(key, texture);
        }
        if self.textures.len() > MAX_CACHED {
            self.textures.clear();
            self.requested.clear();
        }
    }

    pub fn texture(&self, key: &str) -> Option<&egui::TextureHandle> {
        self.textures.get(key)?.as_ref()
    }
}

#[cfg(windows)]
fn start_worker(ctx: &egui::Context, done: Sender<Finished>) -> Option<Sender<Vec<IconRequest>>> {
    let (jobs_tx, jobs_rx) = channel();
    let ctx = ctx.clone();
    std::thread::Builder::new()
        .name("okra-icons".into())
        .spawn(move || {
            win::init_com();
            run_worker(jobs_rx, done, ctx, win::extract);
        })
        .ok()?;
    Some(jobs_tx)
}

#[cfg(not(windows))]
fn start_worker(_ctx: &egui::Context, _done: Sender<Finished>) -> Option<Sender<Vec<IconRequest>>> {
    None
}

/// Fetches icons one at a time. A newer request (what the user is looking at now) goes in front
/// of older, still-unfetched ones, so fast typing never leaves the visible rows waiting behind
/// icons for results that are already gone.
fn run_worker(
    jobs: Receiver<Vec<IconRequest>>,
    done: Sender<Finished>,
    ctx: egui::Context,
    extract: impl Fn(&str) -> Option<egui::ColorImage>,
) {
    let mut pending: VecDeque<IconRequest> = VecDeque::new();
    loop {
        if pending.is_empty() {
            match jobs.recv() {
                Ok(batch) => pending.extend(batch),
                Err(_) => return,
            }
        }
        while let Ok(batch) = jobs.try_recv() {
            for request in batch.into_iter().rev() {
                pending.push_front(request);
            }
        }
        let Some((key, source)) = pending.pop_front() else { continue };
        let image = extract(&source);
        if image.is_none() {
            crate::debug_log(&format!("no icon for {source}"));
        }
        if done.send((key, image)).is_err() {
            return;
        }
        ctx.request_repaint();
    }
}

/// Converts the shell's BGRA bitmap bits to straight-alpha RGBA.
///
/// The shell's 32-bit bitmaps carry alpha but may be premultiplied. If any pixel has a colour
/// channel above its alpha the data cannot be premultiplied; otherwise it is treated as
/// premultiplied (for fully opaque pixels the two are identical anyway).
fn bgra_to_image(width: usize, height: usize, bgra: &[u8]) -> Option<egui::ColorImage> {
    let len = width.checked_mul(height)?.checked_mul(4)?;
    if len == 0 || bgra.len() < len {
        return None;
    }
    let pixels = &bgra[..len];
    if pixels.chunks_exact(4).all(|p| p[3] == 0) {
        return None; // entirely transparent: nothing usable
    }
    let premultiplied = pixels.chunks_exact(4).all(|p| p[0] <= p[3] && p[1] <= p[3] && p[2] <= p[3]);
    let mut rgba = Vec::with_capacity(len);
    for p in pixels.chunks_exact(4) {
        let alpha = p[3] as u32;
        let straight = |c: u8| -> u8 {
            if premultiplied && alpha > 0 && alpha < 255 {
                ((c as u32 * 255 + alpha / 2) / alpha).min(255) as u8
            } else {
                c
            }
        };
        rgba.extend_from_slice(&[straight(p[2]), straight(p[1]), straight(p[0]), p[3]]);
    }
    Some(egui::ColorImage::from_rgba_unmultiplied([width, height], &rgba))
}

#[cfg(windows)]
mod win {
    use super::bgra_to_image;
    use eframe::egui;
    use std::ffi::c_void;
    use windows::core::{ComInterface, PCWSTR};
    use windows::Win32::Foundation::SIZE;
    use windows::Win32::Graphics::Gdi::{
        DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
        HGDIOBJ,
    };
    use windows::Win32::System::Com::{CoInitializeEx, IBindCtx, COINIT_APARTMENTTHREADED};
    use windows::Win32::UI::Shell::{IShellItem, IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_ICONONLY};

    /// Pixels requested from the shell; the rows draw it at about 34 points.
    const ICON_PX: i32 = 64;

    /// The shell's image factory wants a COM apartment on the calling thread.
    pub fn init_com() {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        }
    }

    /// `source` is a file path or a shell parsing name such as `shell:AppsFolder\<AppUserModelID>`.
    pub fn extract(source: &str) -> Option<egui::ColorImage> {
        unsafe {
            let wide: Vec<u16> = source.encode_utf16().chain(std::iter::once(0)).collect();
            let item: IShellItem = SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None::<&IBindCtx>)
                .map_err(|e| crate::debug_log(&format!("icon: cannot open {source}: {e}")))
                .ok()?;
            let factory = item
                .cast::<IShellItemImageFactory>()
                .map_err(|e| crate::debug_log(&format!("icon: no image factory for {source}: {e}")))
                .ok()?;
            let bitmap = factory
                .GetImage(SIZE { cx: ICON_PX, cy: ICON_PX }, SIIGBF_ICONONLY)
                .map_err(|e| crate::debug_log(&format!("icon: GetImage failed for {source}: {e}")))
                .ok()?;
            let image = bitmap_to_image(HGDIOBJ(bitmap.0));
            let _ = DeleteObject(HGDIOBJ(bitmap.0));
            image
        }
    }

    unsafe fn bitmap_to_image(bitmap: HGDIOBJ) -> Option<egui::ColorImage> {
        let mut info = BITMAP::default();
        if GetObjectW(bitmap, std::mem::size_of::<BITMAP>() as i32, Some(&mut info as *mut BITMAP as *mut c_void)) == 0 {
            return None;
        }
        let (width, height) = (info.bmWidth, info.bmHeight);
        if width <= 0 || height <= 0 {
            return None;
        }
        // Negative height asks for top-down rows, which is the order egui wants.
        let mut header = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bgra = vec![0u8; width as usize * height as usize * 4];
        let screen = GetDC(None);
        let lines = GetDIBits(
            screen,
            windows::Win32::Graphics::Gdi::HBITMAP(bitmap.0),
            0,
            height as u32,
            Some(bgra.as_mut_ptr() as *mut c_void),
            &mut header,
            DIB_RGB_COLORS,
        );
        ReleaseDC(None, screen);
        if lines == 0 {
            return None;
        }
        bgra_to_image(width as usize, height as usize, &bgra)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::Duration;

    fn item(kind: Kind, action: Action) -> Item {
        Item { id: "x".into(), title: "x".into(), subtitle: String::new(), kind, action, score: 0 }
    }

    #[test]
    fn apps_files_and_folders_get_the_right_icon_request() {
        let lnk = PathBuf::from(r"C:\ProgramData\Microsoft\Windows\Start Menu\Programs\Ansys 2023 R1\Workbench 2023 R1.lnk");
        let (key, source) = icon_request(&item(Kind::App, Action::Launch { path: lnk.clone(), exec: None })).unwrap();
        assert_eq!(key, lnk.to_string_lossy());
        assert_eq!(source, key);

        let (key, source) = icon_request(&item(Kind::App, Action::LaunchAppId("Microsoft.WindowsCalculator_8wekyb3d8bbwe!App".into()))).unwrap();
        assert_eq!(key, r"shell:AppsFolder\Microsoft.WindowsCalculator_8wekyb3d8bbwe!App");
        assert_eq!(source, key);

        // Documents share one icon per extension (case-insensitive) but ask with a real path...
        let (a, a_src) = icon_request(&item(Kind::File, Action::OpenPath(PathBuf::from(r"C:\Books\One.PDF")))).unwrap();
        let (b, _) = icon_request(&item(Kind::File, Action::OpenPath(PathBuf::from(r"D:\Elsewhere\two.pdf")))).unwrap();
        assert_eq!((a.as_str(), a_src.as_str()), ("ext:.pdf", r"C:\Books\One.PDF"));
        assert_eq!(a, b);
        // ...while programs and shortcuts each keep their own.
        let (e1, _) = icon_request(&item(Kind::File, Action::OpenPath(PathBuf::from(r"C:\a\tool.exe")))).unwrap();
        let (e2, _) = icon_request(&item(Kind::File, Action::OpenPath(PathBuf::from(r"C:\b\other.exe")))).unwrap();
        assert_ne!(e1, e2);
        assert_eq!(icon_request(&item(Kind::Folder, Action::OpenPath(PathBuf::from(r"C:\Users")))).unwrap().0, "folder");

        // Settings, windows, calculations and everything else keep the coloured tile.
        assert!(icon_request(&item(Kind::Setting, Action::OpenUri("ms-settings:bluetooth".into()))).is_none());
        assert!(icon_request(&item(Kind::Window, Action::FocusWindow(1))).is_none());
        assert!(icon_request(&item(Kind::Calculation, Action::Copy("4".into()))).is_none());
    }

    /// egui stores `Color32` premultiplied, so compare in the straight (unmultiplied) space.
    fn straight(image: &egui::ColorImage, index: usize) -> [u8; 4] {
        image.pixels[index].to_srgba_unmultiplied()
    }

    fn close(a: [u8; 4], b: [u8; 4]) -> bool {
        a.iter().zip(&b).all(|(x, y)| (*x as i32 - *y as i32).abs() <= 2)
    }

    #[test]
    fn bitmap_conversion_handles_opaque_premultiplied_and_straight_alpha() {
        // Opaque: BGRA -> RGBA, nothing else changes.
        let img = bgra_to_image(1, 1, &[10, 20, 30, 255]).unwrap();
        assert_eq!(straight(&img, 0), [30, 20, 10, 255]);

        // Premultiplied half-transparent red (B=0, G=0, R=64 at alpha 128) is un-premultiplied
        // back to full-strength red (~128/255) rather than left dark.
        let img = bgra_to_image(1, 2, &[0, 0, 64, 128, 0, 0, 0, 255]).unwrap();
        let red = straight(&img, 0);
        assert!(close(red, [128, 0, 0, 128]), "{red:?}");

        // A colour channel above alpha proves the data is straight alpha: it must not be rescaled.
        let img = bgra_to_image(1, 1, &[200, 0, 0, 100]).unwrap();
        let blue = straight(&img, 0);
        assert!(close(blue, [0, 0, 200, 100]), "{blue:?}");

        // Nothing usable / malformed input.
        assert!(bgra_to_image(2, 1, &[1, 2, 3, 0, 4, 5, 6, 0]).is_none());
        assert!(bgra_to_image(2, 2, &[0; 8]).is_none());
        assert!(bgra_to_image(0, 0, &[]).is_none());
    }

    #[test]
    fn worker_fetches_the_newest_request_before_older_leftovers() {
        let (jobs_tx, jobs_rx) = channel::<Vec<IconRequest>>();
        let (done_tx, done_rx) = channel::<Finished>();
        let slow = |_: &str| {
            std::thread::sleep(Duration::from_millis(60));
            Some(egui::ColorImage::new([1, 1], egui::Color32::WHITE))
        };
        let handle = std::thread::spawn(move || run_worker(jobs_rx, done_tx, egui::Context::default(), slow));
        let req = |n: &str| (n.to_string(), n.to_string());

        jobs_tx.send(vec![req("a1"), req("a2"), req("a3")]).unwrap();
        assert_eq!(done_rx.recv_timeout(Duration::from_secs(5)).unwrap().0, "a1");
        // While a2 is being fetched, the user types again: a new result list arrives.
        jobs_tx.send(vec![req("b1"), req("b2")]).unwrap();
        let order: Vec<String> = (0..4).map(|_| done_rx.recv_timeout(Duration::from_secs(5)).unwrap().0).collect();
        let pos = |k: &str| order.iter().position(|o| o == k).unwrap();
        assert!(pos("b1") < pos("b2"), "row order inside a batch is kept: {order:?}");
        assert!(pos("b2") < pos("a3"), "newest batch jumps ahead of older leftovers: {order:?}");
        drop(jobs_tx);
        handle.join().unwrap();
    }

    #[test]
    fn cache_requests_each_icon_once_and_remembers_failures() {
        let mut icons = Icons::new(&egui::Context::default());
        // Install a fake job channel so the dedupe logic can be observed on any platform.
        let (tx, rx) = channel();
        icons.jobs = Some(tx);
        let wanted = vec![("k1".to_string(), "p1".to_string()), ("k2".to_string(), "p2".to_string())];
        icons.request(wanted.iter());
        icons.request(wanted.iter());
        assert_eq!(rx.try_recv().unwrap().len(), 2);
        assert!(rx.try_recv().is_err(), "second identical request must be a no-op");
        // A failed lookup is cached as `None` and never asked for again.
        icons.textures.insert("k3".into(), None);
        icons.request([("k3".to_string(), "p3".to_string())].iter());
        assert!(rx.try_recv().is_err());
        assert!(icons.texture("k3").is_none());
    }
}
