//! The window over the scripted worked example in skelter-test's headless harness: the panes fill
//! in from background jobs, the debugger keys move the cursor, back restores it, a click on a local
//! traces its origin, the call tree pages in, and a pasted position URL opens its stop.

use hopper_core::api::OwnershipState;
use hopper_core::Moment;
use hopper_gui::run::{Harness, harness, press, settle};
use hopper_gui::tree::Row;
use skelter_base::event::Modifiers;
use skelter_base::geom::Point;
use skelter_base::key::Key;

fn window() -> Harness {
    harness(hopper_fake::FakeDebugger::shared(), (1440.0, 900.0))
}

fn cursor(h: &Harness) -> Moment {
    h.state().viewer.stop.as_ref().expect("a stop").at
}

fn line(h: &Harness) -> u32 {
    h.state().viewer.stop.as_ref().and_then(|s| s.location.as_ref()).map_or(0, |l| l.line)
}

/// A press in the middle of row `row` of a rows pane.
fn click_row(h: &mut Harness, node: skelter_base::ids::NodeId, row: usize, x: f32) {
    let r = h.state().doc().world_rect(node).expect("the pane is laid out");
    let p = Point::new(r.min.x + x, r.min.y + hopper_gui::rows::ROW_H * (row as f32 + 0.5));
    h.click_at(p);
    settle(h);
}

#[test]
fn the_panes_fill_in_from_the_engine() {
    let h = window();
    let v = &h.state().viewer;
    assert!(v.stop.is_some(), "the cursor arrived");
    assert!(v.source.file.as_deref().is_some_and(|f| f.ends_with("main.rs")), "the source is the cursor's file: {:?}", v.source.file);
    assert!(!v.frames.is_empty(), "the stack arrived");
    assert!(v.timeline.start.is_some() && !v.timeline.events.is_empty(), "the timeline arrived");
    assert!(!v.calls.borrow().is_empty(), "the call roots arrived");
    assert!(v.index.as_ref().is_some_and(|i| i.done), "the index status arrived");
    // the source pane drew its lines and asked for their hit counts
    let file = v.source.file.clone().unwrap();
    let hits = v.source.hits.get(&file).expect("hit counts were asked for");
    assert!(matches!(hits.get(line(&h)), Some(hopper_gui::gutter::HitCount::Known { count, .. }) if count > 0), "the stopped line ran");
    // the execution arrow is on the stopped line
    assert!(v.source.arrows.iter().any(|a| a.line == line(&h) && a.arrow == hopper_gui::gutter::Arrow::Execution));
}

#[test]
fn f10_steps_and_alt_left_comes_back() {
    let mut h = window();
    let start = cursor(&h);
    press(&mut h, "F10").unwrap();
    let after = cursor(&h);
    assert_ne!(after, start, "F10 moved the cursor");
    assert!(h.state().viewer.nav.can_back());
    press(&mut h, "F10").unwrap();
    let second = cursor(&h);
    assert!(after.is_before(&second));
    press(&mut h, "Ctrl+F10").unwrap();
    assert_eq!(cursor(&h), after, "Ctrl+F10 steps back over");
    press(&mut h, "Alt+Left").unwrap();
    assert_eq!(cursor(&h), second, "back restores the stop before the reverse step");
    press(&mut h, "Alt+Right").unwrap();
    assert_eq!(cursor(&h), after, "forward redoes it");
    // the locals follow the cursor
    assert!(!h.state().viewer.locals.is_empty());
}

#[test]
fn clicking_a_moved_out_local_shows_its_origin_chain() {
    let mut h = window();
    // step until `a` has been moved out (the move is on line 13)
    for _ in 0..12 {
        let moved = h.state().viewer.locals.rows().iter().any(|r| match r {
            Row::Node { key, .. } => h.state().viewer.locals.item(key).is_some_and(|v| v.name == "a" && matches!(v.ownership, OwnershipState::MovedOut { .. })),
            _ => false,
        });
        if moved {
            break;
        }
        press(&mut h, "F10").unwrap();
    }
    let v = &h.state().viewer;
    let row = v
        .locals
        .rows()
        .iter()
        .position(|r| matches!(r, Row::Node { key, .. } if v.locals.item(key).is_some_and(|x| x.name == "a")))
        .expect("a is a local");
    let val = match &v.locals.rows()[row] {
        Row::Node { key, .. } => v.locals.item(key).unwrap().clone(),
        _ => unreachable!(),
    };
    assert!(matches!(val.ownership, OwnershipState::MovedOut { .. }), "a is moved out by line {}", line(&h));
    let (_, tone) = hopper_gui::app::value_summary(&val);
    assert_eq!(tone, hopper_gui::rows::Tone::Muted, "a moved-out value is greyed");
    let locals = h.state().ui.locals;
    click_row(&mut h, locals, row, 60.0);
    let q = &h.state().viewer.query;
    assert!(q.len() > 2, "the origin chain is in the query pane: {q:?}");
    let hops: Vec<_> = q.iter().filter(|l| l.action.is_some() && l.depth == 1).cloned().collect();
    assert!(!hops.is_empty(), "each hop is a row that seeks");
    // labelled hops name their ownership kind and confidence
    let text: Vec<String> = q.iter().flat_map(|l| l.cells.iter().map(|c| c.text.clone())).collect();
    assert!(text.iter().any(|t| t == "exact" || t == "inferred" || t == "heuristic" || t == "machine"), "{text:?}");

    // a press on the triangle expands it through Debugger::children
    let before = h.state().viewer.locals.len();
    click_row(&mut h, locals, row, 11.0);
    assert!(h.state().viewer.locals.len() > before, "a's fields were fetched and shown");

    // activating a hop seeks to its moment
    let target = match &hops[0].action {
        Some(hopper_gui::app::QueryAction::Seek(m)) => *m,
        None => unreachable!(),
    };
    let hop_row = h.state().viewer.query.iter().position(|l| l.depth == 1 && l.action.is_some()).unwrap();
    h.state_mut().viewer.query_activate(hop_row);
    settle(&mut h);
    assert_eq!(cursor(&h), target);
}

#[test]
fn the_call_tree_expands_lazily() {
    let mut h = window();
    let roots = h.state().viewer.calls.borrow().len();
    assert!(roots >= 1);
    h.state_mut().viewer.activate_call(0);
    settle(&mut h);
    assert!(h.state().viewer.calls.borrow().len() > roots, "main's calls were paged in");
    h.state_mut().viewer.activate_call(0);
    settle(&mut h);
    assert_eq!(h.state().viewer.calls.borrow().len(), roots, "and collapse again");
}

#[test]
fn a_pasted_position_url_opens_its_stop() {
    let mut h = window();
    press(&mut h, "F10").unwrap();
    press(&mut h, "F10").unwrap();
    let there = cursor(&h);
    press(&mut h, "Ctrl+Shift+C").unwrap();
    assert!(h.state().viewer.message.as_ref().is_some_and(|(m, _)| m.contains(&there.url().to_string())));
    press(&mut h, "Alt+Left").unwrap();
    press(&mut h, "Alt+Left").unwrap();
    assert_ne!(cursor(&h), there);
    press(&mut h, "Ctrl+L").unwrap();
    assert!(matches!(h.state().viewer.url_box, hopper_gui::app::UrlBox::Open(_)));
    // replace the box's text with the URL and press Enter
    h.key_press_modifiers(Modifiers::CTRL, Key::A);
    h.type_text(&there.url().to_string());
    h.key_press(Key::Enter);
    settle(&mut h);
    assert_eq!(cursor(&h), there, "the URL's stop is open");
    assert_eq!(h.state().viewer.url_box, hopper_gui::app::UrlBox::Closed);
}

/// With `HOPPER_GUI_SHOT=<out.png>`, renders the window after a moved-out local's origin was
/// traced and its fields expanded (lavapipe in a container); otherwise does nothing.
#[test]
fn shot_of_an_origin_trace() {
    let Ok(out) = std::env::var("HOPPER_GUI_SHOT") else { return };
    let mut h = window();
    for _ in 0..6 {
        press(&mut h, "F10").unwrap();
    }
    let locals = h.state().ui.locals;
    click_row(&mut h, locals, 0, 60.0);
    click_row(&mut h, locals, 0, 11.0);
    h.state_mut().viewer.activate_call(0);
    settle(&mut h);
    let img = h.render().expect("a wgpu adapter");
    img.save_png(std::path::Path::new(&out)).unwrap();
}

#[test]
fn index_progress_is_polled_until_the_index_completes() {
    use hopper_fake::{FakeConfig, FakeDebugger, IndexSchedule};
    let fake = FakeDebugger::with_config(FakeConfig { index: IndexSchedule::AfterPolls(3), ..FakeConfig::default() });
    let h = harness(std::sync::Arc::new(fake), (1200.0, 800.0));
    let v = &h.state().viewer;
    assert!(v.index.as_ref().is_some_and(|i| i.done), "{:?}", v.index);
    assert_eq!(v.index_text(), "index complete");
}
