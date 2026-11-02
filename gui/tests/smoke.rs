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

/// A press in the gutter's marker zone adds a breakpoint on that line and a second press removes
/// it; F9 toggles one on the stopped line; continue stops at a breakpoint.
#[test]
fn breakpoints_from_the_gutter_and_f9_and_continue_stops_there() {
    let mut h = window();
    // the stopped line is revealed at the pane's centre: press the marker zone there
    let source = h.state().ui.source;
    let r = h.state().doc().world_rect(source).expect("the source pane is laid out");
    let p = Point::new(r.min.x + hopper_gui::source::COLUMN_W - 10.0, (r.min.y + r.max.y) / 2.0);
    h.click_at(p);
    settle(&mut h);
    let bps = h.state().viewer.breakpoints.clone();
    assert_eq!(bps.len(), 1, "{:?}", h.state().viewer.message);
    let lines = h.state().viewer.breakpoint_lines();
    assert_eq!(lines.len(), 1);
    // the source pane draws it
    let shown = h
        .state_mut()
        .doc_mut()
        .service::<hopper_gui::source::SourceHost>()
        .and_then(|s| s.panes.get(&source).map(|p| p.breakpoints.clone()))
        .unwrap();
    assert_eq!(shown, lines);
    h.click_at(p);
    settle(&mut h);
    assert!(h.state().viewer.breakpoints.is_empty(), "the second press removed it");

    // F9 on the stopped line (the source has no caret yet), then continue from the start stops
    // there after stepping past it.
    for _ in 0..3 {
        press(&mut h, "F10").unwrap();
    }
    let at = line(&h);
    press(&mut h, "F9").unwrap();
    assert_eq!(h.state().viewer.breakpoint_lines(), vec![at], "{:?} caret {:?}", h.state().viewer.message, h.state().viewer.source.caret_line);
    press(&mut h, "F10").unwrap();
    press(&mut h, "F10").unwrap();
    assert_ne!(line(&h), at);
    press(&mut h, "Shift+F5").unwrap();
    assert_eq!(line(&h), at, "reverse continue stops at the breakpoint");
    assert!(matches!(h.state().viewer.stop.as_ref().unwrap().cause, hopper_core::api::StopCause::Breakpoint { .. }));
    press(&mut h, "F9").unwrap();
    assert!(h.state().viewer.breakpoints.is_empty());
}

/// The disassembly pane shows the code from the frame's PC with the PC marked, the registers pane
/// the stopped thread's registers, and the memory pane the stack, then a chosen local's bytes.
#[test]
fn disassembly_registers_and_memory_follow_the_cursor() {
    let mut h = window();
    press(&mut h, "F10").unwrap();
    let v = &h.state().viewer;
    let pc = v.stop.as_ref().unwrap().pc;
    assert_eq!(v.machine.pc, Some(pc));
    assert!(!v.machine.instructions.is_empty(), "{:?}", v.machine.message);
    assert_eq!(v.machine.instructions[0].addr, pc);
    let regs = v.machine.registers.as_ref().expect("registers");
    assert_eq!(regs.pc(), Some(pc));
    let sp = regs.sp().unwrap();
    assert_eq!(v.memory.addr, Some(sp), "memory starts at the stack pointer");
    assert!(!v.memory.bytes.is_empty(), "{:?}", v.memory.message);
    let rows = hopper_gui::app::register_rows(regs);
    assert_eq!(rows[0].0, "rip");

    // selecting a local shows its bytes, its origin and its history
    for _ in 0..8 {
        press(&mut h, "F10").unwrap();
    }
    let v = &h.state().viewer;
    let (row, view) = v
        .locals
        .rows()
        .iter()
        .enumerate()
        .find_map(|(i, r)| match r {
            Row::Node { key, .. } => v.locals.item(key).filter(|x| x.name == "b").map(|x| (i, x.clone())),
            _ => None,
        })
        .expect("b is a local");
    let locals = h.state().ui.locals;
    click_row(&mut h, locals, row, 60.0);
    let v = &h.state().viewer;
    let Some(hopper_core::Place::Memory { addr, .. }) = view.place else { panic!("b is in memory") };
    assert_eq!(v.memory.addr, Some(addr));
    assert_eq!(v.memory.label, "b");
    let q = &v.query;
    let heading = |t: &str| q.iter().position(|l| l.depth == 0 && l.cells[0].text.starts_with(t));
    let origin = heading("Where did b").expect("the origin");
    let history = heading("History of b").expect("the history");
    assert!(origin < history);
    let changes: Vec<_> = q[history + 1..].iter().filter(|l| l.action.is_some()).collect();
    assert!(!changes.is_empty(), "b's history has its changes: {q:?}");

    // the address box reads another address
    let other = sp + 16;
    h.state_mut().viewer.memory_address(&format!("{other:#x}"));
    settle(&mut h);
    assert_eq!(h.state().viewer.memory.addr, Some(other));
}

/// The origin of `*n` reads as the plan's table: the borrow of `n` first, then the value's own
/// hops, each source hop with its machine hops and the events underneath it.
#[test]
fn the_origin_view_puts_the_borrow_first_and_groups_machine_hops() {
    use hopper_core::api::Budget;
    let d = hopper_fake::FakeDebugger::shared();
    let mut at = d.cursor().unwrap();
    while at.location.as_ref().is_none_or(|l| l.line != 18) {
        at = d.step(hopper_core::api::StepKind::Over, hopper_core::api::Direction::Forward).unwrap();
    }
    let n = d.eval(at.at, hopper_core::FrameId { tid: at.tid, depth: 0 }, "*n").unwrap();
    let chain = d.origin(&n.handle, Budget::default()).unwrap();
    let groups = hopper_gui::app::source_groups(&chain);
    assert_eq!(groups.len(), 6);
    assert_eq!(groups[0][0].ownership.as_ref().unwrap().kind, hopper_core::api::OwnershipKind::SharedBorrow);
    let lines = hopper_gui::app::origin_lines(&n, Ok(chain.clone()), None);
    let hops: Vec<String> = lines.iter().filter(|l| l.depth == 1 && l.action.is_some()).map(|l| l.cells[0].text.clone()).collect();
    assert_eq!(hops[0], "1  shared borrow");
    assert_eq!(hops.len(), 6, "{hops:?}");
    let text: Vec<String> = lines.iter().flat_map(|l| l.cells.iter().map(|c| c.text.clone())).collect();
    assert!(text.iter().any(|t| t.contains("Config { name, .. }")), "the aggregate under load's return: {text:?}");
    assert!(text.iter().any(|t| t.contains("computed from rbp")), "the borrow's operand: {text:?}");
}
