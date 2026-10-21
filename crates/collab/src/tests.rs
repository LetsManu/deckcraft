//! Two or three replicas edit the sample deck and must end with the same presentation.

use std::sync::Arc;

use deckcraft_engine::Session;
use deckcraft_model::{Presentation, Slide};

use super::*;

fn sample() -> Presentation {
    let mut s = Session::new();
    deckcraft_engine::sample::open_sample(&mut s).unwrap();
    (*s.active().unwrap().doc).clone()
}

/// Everything but `next_id` (each participant has its own range).
fn same(a: &Presentation, b: &Presentation) -> bool {
    let mut a = a.clone();
    let mut b = b.clone();
    a.next_id = 0;
    b.next_id = 0;
    a == b
}

struct Peer {
    c: Collab,
    p: Arc<Presentation>,
}

impl Peer {
    /// A local edit: change the presentation and return the update for the others.
    fn edit(&mut self, f: impl FnOnce(&mut Presentation)) -> Vec<u8> {
        let mut q = (*self.p).clone();
        q.next_id = self.c.next_id();
        f(&mut q);
        self.p = Arc::new(q);
        self.c.local(&self.p).unwrap_or_default()
    }
    fn receive(&mut self, update: &[u8]) {
        let (p, pending) = self.c.remote(update, &self.p).unwrap();
        assert!(pending.is_none(), "local edits are always sent before");
        self.p = p;
    }
}

/// Seed with the sample deck and let `n - 1` more people join.
fn session(n: u32) -> Vec<Peer> {
    let mut first = Collab::new(0, 1);
    let deck = sample();
    let seed = first.seed(&deck);
    let mut p0 = deck.clone();
    p0.next_id = first.next_id();
    let mut peers = vec![Peer { p: Arc::new(p0), c: first }];
    peers[0].c.synced = Some(peers[0].p.clone());
    for i in 1..n {
        let mut c = Collab::new(i, u64::from(i) + 1);
        let p = c.join(std::slice::from_ref(&seed)).unwrap();
        peers.push(Peer { c, p: Arc::new(p) });
    }
    peers
}

fn slide_mut(p: &mut Presentation, i: usize) -> &mut Slide {
    Arc::make_mut(&mut p.slides[i])
}

#[test]
fn joining_gives_the_same_presentation() {
    let peers = session(2);
    assert!(peers[0].p.slides.len() > 1);
    assert!(same(&peers[0].p, &peers[1].p));
}

#[test]
fn edits_to_different_shapes_both_survive() {
    let mut peers = session(2);
    let a = peers[0].edit(|p| {
        let sh = &mut slide_mut(p, 0).shapes[0];
        sh.name = "Moved by A".into();
    });
    let b = peers[1].edit(|p| {
        let sh = &mut slide_mut(p, 0).shapes[1];
        sh.name = "Renamed by B".into();
    });
    peers[0].receive(&b);
    peers[1].receive(&a);
    assert!(same(&peers[0].p, &peers[1].p));
    assert_eq!(peers[0].p.slides[0].shapes[0].name, "Moved by A");
    assert_eq!(peers[0].p.slides[0].shapes[1].name, "Renamed by B");
}

#[test]
fn shapes_added_at_the_same_time_are_both_kept() {
    let mut peers = session(2);
    let add = |peer: &mut Peer, name: &str| {
        let name = name.to_string();
        peer.edit(move |p| {
            let id = p.alloc_id().unwrap();
            let mut sh = p.slides[0].shapes[0].clone();
            sh.id = deckcraft_model::ShapeId(id);
            sh.name = name;
            slide_mut(p, 0).shapes.push(sh);
        })
    };
    let a = add(&mut peers[0], "A's shape");
    let b = add(&mut peers[1], "B's shape");
    peers[0].receive(&b);
    peers[1].receive(&a);
    assert!(same(&peers[0].p, &peers[1].p));
    let names: Vec<_> = peers[0].p.slides[0].shapes.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"A's shape") && names.contains(&"B's shape"), "{names:?}");
    let ids: std::collections::HashSet<_> = peers[0].p.slides[0].shapes.iter().map(|s| s.id).collect();
    assert_eq!(ids.len(), peers[0].p.slides[0].shapes.len(), "ids never clash");
}

#[test]
fn moving_slides_and_adding_one_at_the_same_time() {
    let mut peers = session(2);
    let n = peers[0].p.slides.len();
    let a = peers[0].edit(|p| {
        let s = p.slides.remove(0);
        p.slides.push(s);
    });
    let b = peers[1].edit(|p| {
        let id = p.alloc_id().unwrap();
        let mut s = (*p.slides[1]).clone();
        s.id = deckcraft_model::SlideId(id);
        s.name = "new".into();
        p.slides.insert(1, Arc::new(s));
    });
    peers[0].receive(&b);
    peers[1].receive(&a);
    assert!(same(&peers[0].p, &peers[1].p));
    assert_eq!(peers[0].p.slides.len(), n + 1);
}

#[test]
fn the_same_shape_ends_with_one_value_everywhere() {
    let mut peers = session(3);
    let a = peers[0].edit(|p| slide_mut(p, 0).shapes[0].name = "A".into());
    let b = peers[1].edit(|p| slide_mut(p, 0).shapes[0].name = "B".into());
    for u in [&a, &b] {
        peers[2].receive(u);
    }
    peers[0].receive(&b);
    peers[1].receive(&a);
    assert!(same(&peers[0].p, &peers[1].p));
    assert!(same(&peers[0].p, &peers[2].p));
}

#[test]
fn deleting_a_slide_wins_over_editing_it() {
    let mut peers = session(2);
    let gone = peers[0].p.slides[1].id;
    let a = peers[0].edit(|p| {
        p.slides.remove(1);
    });
    let b = peers[1].edit(|p| slide_mut(p, 1).shapes[0].name = "edited".into());
    peers[0].receive(&b);
    peers[1].receive(&a);
    assert!(same(&peers[0].p, &peers[1].p));
    assert!(peers[0].p.slides.iter().all(|s| s.id != gone));
}

#[test]
fn untouched_slides_keep_their_arc() {
    let mut peers = session(2);
    let before: Vec<_> = peers[1].p.slides.clone();
    let a = peers[0].edit(|p| slide_mut(p, 0).shapes[0].name = "x".into());
    peers[1].receive(&a);
    assert!(!Arc::ptr_eq(&before[0], &peers[1].p.slides[0]));
    for (old, new) in before.iter().zip(&peers[1].p.slides).skip(1) {
        assert!(Arc::ptr_eq(old, new));
    }
}

#[test]
fn presentation_settings_and_media_travel() {
    let mut peers = session(2);
    let a = peers[0].edit(|p| {
        p.props.title = "Shared title".into();
        p.media.push(deckcraft_model::MediaItem {
            id: deckcraft_model::MediaId(4242),
            name: "dot.png".into(),
            content_type: "image/png".into(),
            data: Arc::new(vec![1, 2, 3]),
            link: None,
        });
    });
    peers[1].receive(&a);
    assert!(same(&peers[0].p, &peers[1].p));
    assert_eq!(peers[1].p.props.title, "Shared title");
}

#[test]
fn a_late_joiner_gets_everything_from_the_compacted_state() {
    let mut peers = session(2);
    let a = peers[0].edit(|p| slide_mut(p, 0).shapes[0].name = "compacted".into());
    peers[1].receive(&a);
    let state = peers[1].c.state();
    let mut late = Collab::new(7, 99);
    let p = late.join(&[state]).unwrap();
    assert!(same(&p, &peers[0].p));
}

#[test]
fn garbage_updates_are_errors_not_crashes() {
    let mut peers = session(1);
    let p = peers[0].p.clone();
    assert!(peers[0].c.remote(&[1, 2, 3, 255], &p).is_err());
    assert!(Collab::new(1, 2).join(&[vec![9, 9, 9]]).is_err());
}

#[test]
fn undo_takes_back_only_my_own_step() {
    let mut peers = session(2);
    let a = peers[0].edit(|p| slide_mut(p, 0).shapes[0].name = "mine".into());
    peers[0].c.step();
    let b = peers[1].edit(|p| slide_mut(p, 0).shapes[1].name = "theirs".into());
    peers[0].receive(&b);
    peers[1].receive(&a);
    let original = peers[1].p.slides[0].shapes[0].name.clone();
    let _ = original;
    let cur = peers[0].p.clone();
    let (p, u) = peers[0].c.undo(&cur, false).unwrap().unwrap();
    peers[0].p = p;
    peers[1].receive(&u);
    assert!(same(&peers[0].p, &peers[1].p));
    assert_ne!(peers[0].p.slides[0].shapes[0].name, "mine");
    assert_eq!(peers[0].p.slides[0].shapes[1].name, "theirs", "the other person's edit stays");
    let cur = peers[0].p.clone();
    let (p, u) = peers[0].c.undo(&cur, true).unwrap().unwrap();
    peers[0].p = p;
    peers[1].receive(&u);
    assert!(same(&peers[0].p, &peers[1].p));
    assert_eq!(peers[1].p.slides[0].shapes[0].name, "mine");
}

#[test]
fn a_shape_deleted_and_added_back_is_shared_again() {
    let mut peers = session(2);
    let shape = peers[0].p.slides[0].shapes[0].clone();
    let a = peers[0].edit(|p| {
        slide_mut(p, 0).shapes.remove(0);
    });
    peers[1].receive(&a);
    let b = peers[1].edit(move |p| slide_mut(p, 0).shapes.insert(0, shape));
    peers[0].receive(&b);
    assert!(same(&peers[0].p, &peers[1].p));
    let a = peers[0].edit(|p| {
        slide_mut(p, 0).shapes.remove(0);
    });
    peers[1].receive(&a);
    assert!(same(&peers[0].p, &peers[1].p));
}

/// The first shape with text on the first slide, as (index, text).
fn text_shape(p: &Presentation) -> usize {
    p.slides[0].shapes.iter().position(|s| s.text.as_ref().is_some_and(|t| !t.text().is_empty())).unwrap()
}

fn text_of(p: &Presentation, i: usize) -> String {
    p.slides[0].shapes[i].text.as_ref().unwrap().text()
}

fn set_text(p: &mut Presentation, i: usize, f: impl FnOnce(&mut String)) {
    let body = slide_mut(p, 0).shapes[i].text.as_mut().unwrap();
    let para = &mut body.paragraphs[0];
    let mut t = para.runs[0].text.clone();
    f(&mut t);
    para.runs[0].text = t;
}

#[test]
fn two_people_typing_in_the_same_text_box_keep_both() {
    let mut peers = session(2);
    let i = text_shape(&peers[0].p);
    let before = text_of(&peers[0].p, i);
    let a = peers[0].edit(|p| set_text(p, i, |t| t.insert_str(0, "A-")));
    let b = peers[1].edit(|p| set_text(p, i, |t| t.push_str("-B")));
    peers[0].receive(&b);
    peers[1].receive(&a);
    assert!(same(&peers[0].p, &peers[1].p));
    let after = text_of(&peers[0].p, i);
    assert!(after.starts_with("A-") && after.contains(&before) && after.split('\n').next().unwrap().ends_with("-B"), "{after:?}");
}

#[test]
fn bold_by_one_and_typing_by_the_other_both_stay() {
    let mut peers = session(2);
    let i = text_shape(&peers[0].p);
    let a = peers[0].edit(|p| {
        let run = &mut slide_mut(p, 0).shapes[i].text.as_mut().unwrap().paragraphs[0].runs[0];
        run.props.bold = Some(true);
    });
    let b = peers[1].edit(|p| set_text(p, i, |t| t.push('!')));
    peers[0].receive(&b);
    peers[1].receive(&a);
    assert!(same(&peers[0].p, &peers[1].p));
    let para = &peers[0].p.slides[0].shapes[i].text.as_ref().unwrap().paragraphs[0];
    assert!(para.text().ends_with('!'));
    assert!(para.runs.iter().any(|r| r.props.bold == Some(true)));
}

#[test]
fn text_round_trips_exactly() {
    // Every text body of the sample deck survives the trip through the shared document.
    let peers = session(2);
    for (a, b) in peers[0].p.slides.iter().zip(&peers[1].p.slides) {
        assert_eq!(a.notes, b.notes);
        for (x, y) in a.shapes.iter().zip(&b.shapes) {
            assert_eq!(x.text, y.text, "shape {}", x.name);
        }
    }
}

#[test]
fn the_caret_moves_with_text_typed_before_it() {
    let mut peers = session(2);
    let i = text_shape(&peers[0].p);
    let slide = peers[0].p.slides[0].id;
    let shape = peers[0].p.slides[0].shapes[i].id;
    let anchors = peers[1].c.text_anchors(slide, Some(shape), &[(0, 2)]);
    let a = peers[0].edit(|p| set_text(p, i, |t| t.insert_str(0, "xyz")));
    peers[1].receive(&a);
    assert_eq!(peers[1].c.text_positions(slide, Some(shape), &anchors), vec![Some((0, 5))]);
}
