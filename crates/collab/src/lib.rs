//! Live co-editing: several people work on one presentation at the same time.
//!
//! The presentation lives in a Yjs document ([yrs]) that every participant holds a replica of. The
//! engine keeps editing its own [`Presentation`] exactly as before; this crate translates between
//! the two:
//!
//! - [`Collab::local`] compares the presentation after a local edit with the last state both sides
//!   agreed on and writes only what changed into the shared document. The returned update goes to
//!   the other participants.
//! - [`Collab::remote`] merges an update from someone else and rebuilds only the parts of the
//!   presentation it touched; everything else keeps its `Arc`s.
//!
//! # Shared document layout
//!
//! | Root | Key | Value |
//! |---|---|---|
//! | `meta` (map) | a `Presentation` field (camelCase) | that field as JSON |
//! | `order` (array) | – | slide ids in show order |
//! | `slides` (map) | slide id | the slide without its shapes, as JSON |
//! | `shapes` (map) | `slide/shape` | one top-level shape (with its group children) as JSON |
//! | `zorder` (map) | slide id | the slide's shape ids, back to front, as JSON |
//! | `media` (map) | media id | the media item without its bytes, as JSON |
//! | `blobs` (map) | media id | the media bytes |
//!
//! Everything a person can change at once (a shape, a slide's own settings, one presentation
//! field) is one entry, so two people changing different shapes never interfere and the same shape
//! ends with the last change. Slide order is a real sequence: moving slides at the same time keeps
//! everyone's slides. A shape or slide whose id is missing from an order list is still kept (put at
//! the end), so concurrent additions are never lost.
//!
//! New shapes and slides need ids nobody else uses at the same time: each participant gets its own
//! range ([`Collab::new`]'s `slot`), and `Presentation::next_id` stays inside it.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]
#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use deckcraft_model::{MediaItem, Presentation, Shape, ShapeId, Slide, SlideId};
use serde_json::Value;
use yrs::types::ToJson;
use yrs::undo::{Options as UndoOptions, UndoManager};
use yrs::updates::decoder::Decode;
use yrs::{Any, Array, ArrayRef, Doc, Map, MapRef, Origin, Out, ReadTxn, StateVector, Transact, TransactionMut, Update};

/// Origin of this replica's own edits: only these are undone by [`Collab::undo`].
const LOCAL: &str = "deckcraft-local";

/// First id of the participant ranges; files rarely come near it.
const ID_BASE: u32 = 0x0100_0000;
/// Ids per participant and session.
const ID_SPAN: u32 = 0x0001_0000;
/// Participant slots (`ID_BASE + SLOTS * ID_SPAN` stays below 2³¹, PowerPoint's slide id limit).
pub const SLOTS: u32 = 0x1000;

#[derive(Debug, thiserror::Error)]
pub enum CollabError {
    #[error("the update from the other participants could not be read: {0}")]
    Update(String),
    #[error("the shared presentation could not be read: {0}")]
    Shape(String),
}

pub type Result<T> = std::result::Result<T, CollabError>;

/// One participant's replica of the shared presentation.
pub struct Collab {
    doc: Doc,
    meta: MapRef,
    order: ArrayRef,
    slides: MapRef,
    shapes: MapRef,
    zorder: MapRef,
    media: MapRef,
    blobs: MapRef,
    /// The presentation as the shared document describes it right now (after the last local diff
    /// or remote merge).
    synced: Option<Arc<Presentation>>,
    /// The value each entry had when we last wrote or read it, to spot what an update changed.
    seen: HashMap<String, Arc<str>>,
    next_id: u32,
    id_floor: u32,
    id_ceiling: u32,
    /// Undo and redo of this participant's own edits only (others' edits stay).
    undo: UndoManager<()>,
}

impl std::fmt::Debug for Collab {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Collab").field("next_id", &self.next_id).finish_non_exhaustive()
    }
}

/// The entry of a shape in the `shapes` map.
fn shape_key(slide: SlideId, shape: ShapeId) -> String {
    format!("{}/{}", slide.0, shape.0)
}

fn to_json<T: serde::Serialize>(v: &T) -> Option<String> {
    serde_json::to_string(v).ok()
}

/// A string entry of a map.
fn get_str<T: ReadTxn>(map: &MapRef, txn: &T, key: &str) -> Option<Arc<str>> {
    match map.get(txn, key) {
        Some(Out::Any(Any::String(s))) => Some(s),
        _ => None,
    }
}

/// The ids a JSON list entry names (`zorder`).
fn id_list(s: &str) -> Vec<u32> {
    serde_json::from_str(s).unwrap_or_default()
}

/// The presentation-level fields that are shared as `meta` entries: everything but the slides, the
/// media (own maps) and `nextId` (every participant has its own).
fn meta_fields(p: &Presentation) -> serde_json::Map<String, Value> {
    match serde_json::to_value(p) {
        Ok(Value::Object(mut m)) => {
            for k in ["slides", "media", "nextId"] {
                m.remove(k);
            }
            m
        }
        _ => serde_json::Map::new(),
    }
}

/// Whether the presentation-level fields of `a` and `b` are the same (without serialising them).
fn same_meta(a: &Presentation, b: &Presentation) -> bool {
    let masters_same = a.masters.len() == b.masters.len() && a.masters.iter().zip(&b.masters).all(|(x, y)| Arc::ptr_eq(x, y) || x == y);
    if !masters_same {
        return false;
    }
    let strip = |p: &Presentation| {
        let mut q = p.clone();
        q.slides.clear();
        q.media.clear();
        q.masters.clear();
        q.next_id = 0;
        q
    };
    strip(a) == strip(b)
}

/// A slide as shared in `slides`: without its shapes.
fn slide_json(s: &Slide) -> Option<String> {
    let mut v = serde_json::to_value(s).ok()?;
    if let Value::Object(m) = &mut v {
        m.remove("shapes");
    }
    serde_json::to_string(&v).ok()
}

/// A media item as shared in `media`: without its bytes (they go to `blobs`).
fn media_json(m: &MediaItem) -> Option<String> {
    to_json(m)
}

fn same_media(a: &MediaItem, b: &MediaItem) -> bool {
    a.id == b.id && a.name == b.name && a.content_type == b.content_type && a.link == b.link && (Arc::ptr_eq(&a.data, &b.data) || a.data == b.data)
}

impl Collab {
    /// A replica for participant `slot` (0..[`SLOTS`], unique among the people in the session,
    /// given out by the server). `client_id` identifies this replica in the CRDT; it must be unique
    /// too (random is fine).
    pub fn new(slot: u32, client_id: u64) -> Self {
        let doc = Doc::with_client_id(client_id & ((1 << 53) - 1));
        let slot = slot % SLOTS;
        let id_floor = ID_BASE + slot * ID_SPAN;
        // One undo step per engine step: everything between two `step` calls is grouped (the clock
        // never moves, so only `reset` ends a group).
        let mut undo = UndoManager::with_options(UndoOptions {
            capture_timeout_millis: u64::MAX,
            tracked_origins: [Origin::from(LOCAL)].into_iter().collect(),
            capture_transaction: None,
            timestamp: Arc::new(|| 1u64),
            init_undo_stack: Vec::new(),
            init_redo_stack: Vec::new(),
        });
        for name in ["meta", "order", "slides", "shapes", "zorder", "media", "blobs"] {
            let root = doc.get_or_insert_map(name);
            if name == "order" {
                undo.expand_scope(&doc, &doc.get_or_insert_array(name));
            } else {
                undo.expand_scope(&doc, &root);
            }
        }
        Collab {
            meta: doc.get_or_insert_map("meta"),
            order: doc.get_or_insert_array("order"),
            slides: doc.get_or_insert_map("slides"),
            shapes: doc.get_or_insert_map("shapes"),
            zorder: doc.get_or_insert_map("zorder"),
            media: doc.get_or_insert_map("media"),
            blobs: doc.get_or_insert_map("blobs"),
            doc,
            synced: None,
            seen: HashMap::new(),
            next_id: id_floor,
            id_floor,
            id_ceiling: id_floor + ID_SPAN,
            undo,
        }
    }

    /// End the current undo step: the next local edit starts a new one.
    pub fn step(&mut self) {
        self.undo.reset();
    }

    pub fn can_undo(&self) -> bool {
        self.undo.can_undo()
    }

    pub fn can_redo(&self) -> bool {
        self.undo.can_redo()
    }

    /// Take back this participant's last step (others' edits stay). Returns the presentation to
    /// show and the update for the others; `None` when there is nothing to undo.
    pub fn undo(&mut self, current: &Arc<Presentation>, redo: bool) -> Result<Option<(Arc<Presentation>, Vec<u8>)>> {
        let mut out = Vec::new();
        if let Some(u) = self.local(current) {
            out.push(u);
        }
        self.step();
        let before = self.doc.transact().state_vector();
        let done = if redo { self.undo.redo_blocking() } else { self.undo.undo_blocking() };
        if !done {
            return Ok(None);
        }
        let update = self.doc.transact().encode_diff_v1(&before);
        let base = self.synced.clone().unwrap_or_else(|| current.clone());
        let mut p = self.materialize(&base)?;
        p.next_id = self.next_id;
        let p = Arc::new(p);
        self.synced = Some(p.clone());
        out.push(update);
        Ok(Some((p, merge(&out))))
    }

    /// The first participant fills the shared document with the presentation it opened; the
    /// update goes to the server, which hands it to everyone joining later.
    pub fn seed(&mut self, p: &Presentation) -> Vec<u8> {
        self.synced = Some(Arc::new(Presentation::default()));
        let mut p = p.clone();
        self.claim_ids(&mut p);
        let update = self.local(&Arc::new(p)).unwrap_or_default();
        // The presentation as opened is not an undo step.
        self.undo.clear_all();
        self.step();
        update
    }

    /// Join a session: merge everything the others did so far and build the presentation.
    pub fn join(&mut self, updates: &[Vec<u8>]) -> Result<Presentation> {
        {
            let mut txn = self.doc.transact_mut();
            for u in updates {
                let update = Update::decode_v1(u).map_err(|e| CollabError::Update(e.to_string()))?;
                txn.apply_update(update).map_err(|e| CollabError::Update(e.to_string()))?;
            }
        }
        let mut p = self.materialize(&Presentation::default())?;
        self.claim_ids(&mut p);
        let p = Arc::new(p);
        self.synced = Some(p.clone());
        Ok(Arc::unwrap_or_clone(p))
    }

    /// The whole shared document as one update (the server keeps it instead of the many small ones).
    pub fn state(&self) -> Vec<u8> {
        self.doc.transact().encode_state_as_update_v1(&StateVector::default())
    }

    /// Keep `next_id` in this participant's range so new shapes and slides never clash.
    fn claim_ids(&mut self, p: &mut Presentation) {
        if (self.id_floor..self.id_ceiling).contains(&p.next_id) {
            self.next_id = self.next_id.max(p.next_id);
        }
        p.next_id = self.next_id;
    }

    /// The id the presentation should continue from after an edit here.
    pub fn next_id(&self) -> u32 {
        self.next_id
    }

    /// Write what changed since the last sync into the shared document. `None`: nothing to send.
    pub fn local(&mut self, p: &Arc<Presentation>) -> Option<Vec<u8>> {
        let old = self.synced.clone().unwrap_or_default();
        if Arc::ptr_eq(&old, p) {
            return None;
        }
        if (self.id_floor..self.id_ceiling).contains(&p.next_id) {
            self.next_id = self.next_id.max(p.next_id);
        }
        let before = self.doc.transact().state_vector();
        {
            let doc = self.doc.clone();
            let mut txn = doc.transact_mut_with(LOCAL);
            self.write_meta(&mut txn, &old, p);
            self.write_slides(&mut txn, &old, p);
            self.write_media(&mut txn, &old, p);
        }
        self.synced = Some(p.clone());
        let update = self.doc.transact().encode_diff_v1(&before);
        // An empty update still carries its (empty) delete set: two bytes.
        (update.len() > 2).then_some(update)
    }

    /// Set `key` of `map` to `json` unless it already is; `kind` names the map in [`Self::seen`].
    fn put(&mut self, txn: &mut TransactionMut, map: &MapRef, kind: &str, key: &str, json: String) {
        let value: Arc<str> = Arc::from(json);
        let seen_key = format!("{kind}:{key}");
        if self.seen.get(&seen_key).is_some_and(|s| *s == value) {
            return;
        }
        map.insert(txn, key, Any::String(value.clone()));
        self.seen.insert(seen_key, value);
    }

    fn drop_key(&mut self, txn: &mut TransactionMut, map: &MapRef, kind: &str, key: &str) {
        map.remove(txn, key);
        self.seen.remove(&format!("{kind}:{key}"));
    }

    fn write_meta(&mut self, txn: &mut TransactionMut, old: &Presentation, new: &Presentation) {
        if self.seen.contains_key("meta:slideSize") && same_meta(old, new) {
            return;
        }
        let meta = self.meta.clone();
        for (k, v) in meta_fields(new) {
            if let Ok(s) = serde_json::to_string(&v) {
                self.put(txn, &meta, "meta", &k, s);
            }
        }
    }

    fn write_slides(&mut self, txn: &mut TransactionMut, old: &Presentation, new: &Presentation) {
        let old_slides: HashMap<SlideId, &Arc<Slide>> = old.slides.iter().map(|s| (s.id, s)).collect();
        let new_ids: HashSet<SlideId> = new.slides.iter().map(|s| s.id).collect();
        for s in &new.slides {
            let prev = old_slides.get(&s.id);
            if prev.is_some_and(|o| Arc::ptr_eq(o, s)) {
                continue;
            }
            if let Some(json) = slide_json(s) {
                let slides = self.slides.clone();
                self.put(txn, &slides, "slide", &s.id.0.to_string(), json);
            }
            let prev_shapes: HashMap<ShapeId, &Shape> = prev.map(|o| o.shapes.iter().map(|sh| (sh.id, sh)).collect()).unwrap_or_default();
            let mut ids = Vec::with_capacity(s.shapes.len());
            for sh in &s.shapes {
                ids.push(sh.id.0);
                if prev_shapes.get(&sh.id).is_some_and(|o| *o == sh) && self.seen.contains_key(&format!("shape:{}", shape_key(s.id, sh.id))) {
                    continue;
                }
                if let Some(json) = to_json(sh) {
                    let shapes = self.shapes.clone();
                    self.put(txn, &shapes, "shape", &shape_key(s.id, sh.id), json);
                }
            }
            let kept: HashSet<u32> = ids.iter().copied().collect();
            let shapes = self.shapes.clone();
            for gone in prev_shapes.keys().filter(|id| !kept.contains(&id.0)) {
                self.drop_key(txn, &shapes, "shape", &shape_key(s.id, *gone));
            }
            if let Some(json) = to_json(&ids) {
                let zorder = self.zorder.clone();
                self.put(txn, &zorder, "zorder", &s.id.0.to_string(), json);
            }
        }
        let (slides, zorder, shapes) = (self.slides.clone(), self.zorder.clone(), self.shapes.clone());
        for o in old.slides.iter().filter(|o| !new_ids.contains(&o.id)) {
            let key = o.id.0.to_string();
            self.drop_key(txn, &slides, "slide", &key);
            self.drop_key(txn, &zorder, "zorder", &key);
            for sh in &o.shapes {
                self.drop_key(txn, &shapes, "shape", &shape_key(o.id, sh.id));
            }
        }
        let target: Vec<u32> = new.slides.iter().map(|s| s.id.0).collect();
        self.write_order(txn, &target);
    }

    /// Edit the shared slide order into `target` with as few moves as possible, so concurrent
    /// moves and insertions by others survive.
    fn write_order(&self, txn: &mut TransactionMut, target: &[u32]) {
        let mut cur: Vec<u32> = self.order.iter(txn).filter_map(|v| v.to_json(txn).try_into_u32()).collect();
        if cur == target {
            return;
        }
        let wanted: HashSet<u32> = target.iter().copied().collect();
        let mut i = 0usize;
        while i < cur.len() {
            if cur.get(i).is_some_and(|id| !wanted.contains(id)) || cur.get(..i).is_some_and(|before| before.contains(&cur[i])) {
                self.order.remove(txn, i as u32);
                cur.remove(i);
            } else {
                i += 1;
            }
        }
        for (pos, id) in target.iter().enumerate() {
            if cur.get(pos) == Some(id) {
                continue;
            }
            if let Some(at) = cur.iter().position(|c| c == id) {
                self.order.remove(txn, at as u32);
                cur.remove(at);
            }
            let pos = pos.min(cur.len());
            self.order.insert(txn, pos as u32, Any::from(i64::from(*id)));
            cur.insert(pos, *id);
        }
        while cur.len() > target.len() {
            let last = cur.len() - 1;
            self.order.remove(txn, last as u32);
            cur.pop();
        }
    }

    fn write_media(&mut self, txn: &mut TransactionMut, old: &Presentation, new: &Presentation) {
        let old_media: HashMap<_, &MediaItem> = old.media.iter().map(|m| (m.id, m)).collect();
        let new_ids: HashSet<_> = new.media.iter().map(|m| m.id).collect();
        for m in &new.media {
            if old_media.get(&m.id).is_some_and(|o| same_media(o, m)) {
                continue;
            }
            let key = m.id.0.to_string();
            if let Some(json) = media_json(m) {
                let media = self.media.clone();
                self.put(txn, &media, "media", &key, json);
            }
            if !old_media.get(&m.id).is_some_and(|o| Arc::ptr_eq(&o.data, &m.data) || o.data == m.data) {
                self.blobs.insert(txn, key.as_str(), Any::Buffer(Arc::from(m.data.as_slice())));
            }
        }
        let media = self.media.clone();
        for o in old.media.iter().filter(|o| !new_ids.contains(&o.id)) {
            let key = o.id.0.to_string();
            self.drop_key(txn, &media, "media", &key);
            self.blobs.remove(txn, key.as_str());
        }
    }

    /// Merge an update from another participant. `current` is the presentation shown right now;
    /// local edits in it that were not sent yet are written first (and returned, to send). The
    /// result keeps every untouched slide's `Arc`.
    pub fn remote(&mut self, update: &[u8], current: &Arc<Presentation>) -> Result<(Arc<Presentation>, Option<Vec<u8>>)> {
        let pending = self.local(current);
        {
            let update = Update::decode_v1(update).map_err(|e| CollabError::Update(e.to_string()))?;
            let mut txn = self.doc.transact_mut();
            txn.apply_update(update).map_err(|e| CollabError::Update(e.to_string()))?;
        }
        let base = self.synced.clone().unwrap_or_else(|| current.clone());
        let mut p = self.materialize(&base)?;
        p.next_id = self.next_id;
        let p = Arc::new(p);
        self.synced = Some(p.clone());
        Ok((p, pending))
    }

    /// Build the presentation the shared document describes, reusing what is unchanged in `base`.
    fn materialize(&mut self, base: &Presentation) -> Result<Presentation> {
        let doc = self.doc.clone();
        let txn = doc.transact();
        // Every entry that still exists; the rest is dropped from `seen` at the end.
        let mut touched: HashSet<String> = HashSet::new();
        // Presentation-level fields.
        let mut meta_changed = false;
        let mut fields = serde_json::Map::new();
        for (k, v) in self.meta.iter(&txn) {
            if let Out::Any(Any::String(s)) = v {
                let key = format!("meta:{k}");
                if !self.seen.get(&key).is_some_and(|old| Arc::ptr_eq(old, &s) || *old == s) {
                    meta_changed = true;
                }
                if let Ok(value) = serde_json::from_str::<Value>(&s) {
                    fields.insert(k.to_string(), value);
                }
                touched.insert(key.clone());
                self.seen.insert(key, s);
            }
        }
        let mut p = if meta_changed || base.masters.is_empty() {
            fields.insert("slides".into(), Value::Array(vec![]));
            fields.insert("media".into(), Value::Array(vec![]));
            serde_json::from_value::<Presentation>(Value::Object(fields)).map_err(|e| CollabError::Shape(e.to_string()))?
        } else {
            let mut q = base.clone();
            q.slides.clear();
            q.media.clear();
            q
        };

        // Slides, in the shared order; slides missing from it go to the end (by id).
        let mut ids: Vec<u32> = Vec::new();
        let mut listed = HashSet::new();
        for v in self.order.iter(&txn) {
            if let Some(id) = v.to_json(&txn).try_into_u32()
                && listed.insert(id)
            {
                ids.push(id);
            }
        }
        let mut extra: Vec<u32> = self.slides.keys(&txn).filter_map(|k| k.parse().ok()).filter(|id| !listed.contains(id)).collect();
        extra.sort_unstable();
        ids.extend(extra);

        let base_slides: HashMap<SlideId, &Arc<Slide>> = base.slides.iter().map(|s| (s.id, s)).collect();
        // Shapes per slide, by id.
        let mut shape_entries: HashMap<u32, Vec<(u32, Arc<str>)>> = HashMap::new();
        for (k, v) in self.shapes.iter(&txn) {
            let Out::Any(Any::String(s)) = v else { continue };
            let Some((slide, shape)) = k.split_once('/') else { continue };
            let (Ok(slide), Ok(shape)) = (slide.parse::<u32>(), shape.parse::<u32>()) else { continue };
            shape_entries.entry(slide).or_default().push((shape, s));
        }
        for id in ids {
            let Some(json) = get_str(&self.slides, &txn, &id.to_string()) else { continue };
            let sid = SlideId(id);
            let slide_key = format!("slide:{id}");
            let mut changed = !self.seen.get(&slide_key).is_some_and(|old| Arc::ptr_eq(old, &json) || *old == json);
            let order_json = get_str(&self.zorder, &txn, &id.to_string());
            let zorder_key = format!("zorder:{id}");
            if let Some(z) = &order_json {
                changed |= !self.seen.get(&zorder_key).is_some_and(|old| Arc::ptr_eq(old, z) || *old == *z);
            }
            let entries = shape_entries.remove(&id).unwrap_or_default();
            touched.insert(slide_key.clone());
            if order_json.is_some() {
                touched.insert(zorder_key.clone());
            }
            for (shape, _) in &entries {
                touched.insert(format!("shape:{}", shape_key(sid, ShapeId(*shape))));
            }
            let prev = base_slides.get(&sid);
            let prev_ids: HashSet<u32> = prev.map(|s| s.shapes.iter().map(|sh| sh.id.0).collect()).unwrap_or_default();
            changed |= prev.is_none() || entries.len() != prev_ids.len();
            for (shape, s) in &entries {
                let key = format!("shape:{}", shape_key(sid, ShapeId(*shape)));
                if !self.seen.get(&key).is_some_and(|old| Arc::ptr_eq(old, s) || *old == *s) || !prev_ids.contains(shape) {
                    changed = true;
                }
            }
            if !changed && let Some(prev) = prev {
                p.slides.push((*prev).clone());
                continue;
            }
            // Rebuild this slide.
            let mut slide: Slide = serde_json::from_str(&json).map_err(|e| CollabError::Shape(e.to_string()))?;
            let prev_shapes: HashMap<u32, &Shape> = prev.map(|s| s.shapes.iter().map(|sh| (sh.id.0, sh)).collect()).unwrap_or_default();
            let mut by_id: HashMap<u32, Shape> = HashMap::new();
            for (shape, s) in &entries {
                let key = format!("shape:{}", shape_key(sid, ShapeId(*shape)));
                let same = self.seen.get(&key).is_some_and(|old| Arc::ptr_eq(old, s) || *old == *s);
                let parsed = match prev_shapes.get(shape) {
                    Some(sh) if same => Some((*sh).clone()),
                    _ => serde_json::from_str::<Shape>(s).ok(),
                };
                if let Some(sh) = parsed {
                    by_id.insert(*shape, sh);
                }
                self.seen.insert(key, s.clone());
            }
            let mut order = order_json.as_deref().map(id_list).unwrap_or_default();
            let mut rest: Vec<u32> = by_id.keys().copied().filter(|k| !order.contains(k)).collect();
            rest.sort_unstable();
            order.extend(rest);
            slide.shapes = order.into_iter().filter_map(|k| by_id.remove(&k)).collect();
            slide.id = sid;
            self.seen.insert(slide_key, json);
            if let Some(z) = order_json {
                self.seen.insert(zorder_key, z);
            }
            p.slides.push(Arc::new(slide));
        }

        // Media (bytes from `blobs`; unchanged ones keep their `Arc` from `base`).
        let mut media: Vec<MediaItem> = Vec::new();
        for (k, v) in self.media.iter(&txn) {
            let Out::Any(Any::String(s)) = v else { continue };
            let Ok(mut item) = serde_json::from_str::<MediaItem>(&s) else { continue };
            let prev = base.media.iter().find(|m| m.id == item.id);
            match self.blobs.get(&txn, k) {
                Some(Out::Any(Any::Buffer(b))) => {
                    item.data = match prev {
                        Some(p) if p.data.as_slice() == &*b => p.data.clone(),
                        _ => Arc::new(b.to_vec()),
                    }
                }
                _ => {
                    if let Some(p) = prev {
                        item.data = p.data.clone();
                    }
                }
            }
            touched.insert(format!("media:{k}"));
            self.seen.insert(format!("media:{k}"), s);
            media.push(item);
        }
        self.seen.retain(|k, _| touched.contains(k));
        media.sort_by_key(|m| m.id.0);
        p.media = media;
        Ok(p)
    }
}

/// Several updates as one.
pub fn merge(updates: &[Vec<u8>]) -> Vec<u8> {
    match updates {
        [one] => one.clone(),
        _ => yrs::merge_updates_v1(updates).unwrap_or_else(|_| updates.concat()),
    }
}

trait TryIntoU32 {
    fn try_into_u32(self) -> Option<u32>;
}

impl TryIntoU32 for Any {
    fn try_into_u32(self) -> Option<u32> {
        match self {
            Any::Number(n) => n.as_i64().and_then(|n| u32::try_from(n).ok()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests;
