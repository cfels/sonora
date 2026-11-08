use super::*;
use std::cell::RefCell;
use std::collections::{HashSet, VecDeque};
use std::rc::Rc;

const COVER_LIMIT: usize = 256;
const PENDING_LIMIT: usize = 32;
const ROW_COVER_PX: u32 = 96;

#[derive(Default)]
struct Covers {
    ready: HashMap<String, Option<slint::Image>>,
    pending: HashSet<String>,
    order: VecDeque<String>,
}

pub(super) fn wire(
    ui: &AppWindow,
    weak: Weak<AppWindow>,
    lookup: Arc<Mutex<TrackLookup>>,
    handle: Handle,
) {
    let covers = Rc::new(RefCell::new(Covers::default()));
    let gate = Arc::new(tokio::sync::Semaphore::new(2));
    {
        let covers = covers.clone();
        ui.global::<RowArtwork>().on_image_for_key(move |key, _| {
            covers
                .borrow()
                .ready
                .get(key.as_str())
                .and_then(Clone::clone)
                .unwrap_or_default()
        });
    }
    {
        let covers = covers.clone();
        let weak = weak.clone();
        ui.global::<RowArtwork>().on_loaded(move |key, image| {
            let mut cache = covers.borrow_mut();
            cache.pending.remove(key.as_str());
            if cache.ready.len() >= COVER_LIMIT {
                if let Some(oldest) = cache.order.pop_front() {
                    cache.ready.remove(&oldest);
                }
            }
            cache.order.push_back(key.to_string());
            cache
                .ready
                .insert(key.to_string(), (image.size().width > 0).then_some(image));
            drop(cache);
            if let Some(ui) = weak.upgrade() {
                let global = ui.global::<RowArtwork>();
                global.set_revision(global.get_revision().wrapping_add(1));
            }
        });
    }
    ui.global::<RowArtwork>().on_requested(move |id, key| {
        let key = key.to_string();
        if key.is_empty() {
            return;
        }
        {
            let mut cache = covers.borrow_mut();
            if cache.ready.contains_key(&key)
                || cache.pending.len() >= PENDING_LIMIT
                || !cache.pending.insert(key.clone())
            {
                return;
            }
        }
        let path = lock(&lookup).paths.get(&i64::from(id)).cloned();
        let Some(path) = path else {
            covers.borrow_mut().pending.remove(&key);
            return;
        };
        let weak = weak.clone();
        let gate = gate.clone();
        handle.spawn(async move {
            let Ok(_permit) = gate.acquire_owned().await else {
                return;
            };
            let pixels =
                tokio::task::spawn_blocking(move || cover_pixels(&path, ROW_COVER_PX)).await;
            let _ = weak.upgrade_in_event_loop(move |ui| {
                let image = match pixels {
                    Ok(pixels) => pixel_image(pixels),
                    Err(err) => {
                        log::warn!("row artwork task failed: {err}");
                        slint::Image::default()
                    }
                };
                ui.global::<RowArtwork>().invoke_loaded(key.into(), image);
            });
        });
    });
}
