// SPDX-License-Identifier: GPL-3.0-or-later
use slint::{ComponentHandle, VecModel};
use std::{cell::RefCell, rc::Rc};
slint::slint! {
    import { CardGrid } from "../ui/components/cards.slint";
    import { CardItem, Actions, Route } from "../ui/globals/bridge.slint";
    export { Actions }
    export component GridProbe inherits Window {
        width: 800px;
        height: 400px;
        in property <[CardItem]> cards;
        in-out property <length> top;
        out property <int> count: grid.instantiated-count;
        out property <int> columns: grid.columns;
        out property <length> row-height: grid.row-height;
        grid := CardGrid {
            y: -root.top;
            width: root.width;
            cards: root.cards;
            top: root.top;
            view-height: root.height;
            anchor-changed(top) => { root.top = top; }
        }
    }
}
pub(crate) fn visible_cards_keep_source_actions() {
    let probe = GridProbe::new().unwrap();
    probe.set_cards(
        Rc::new(VecModel::from(
            (0..1000)
                .map(|n| CardItem {
                    id: n.to_string().into(),
                    key: n.to_string().into(),
                    title: format!("Card {n}").into(),
                    route: Route::Album,
                    ..Default::default()
                })
                .collect::<Vec<_>>(),
        ))
        .into(),
    );
    let opened = Rc::new(RefCell::new(String::new()));
    probe.global::<Actions>().on_collection({
        let opened = opened.clone();
        move |_, id| *opened.borrow_mut() = id.to_string()
    });
    probe.show().unwrap();
    let check_action = || {
        let top = probe.get_top();
        let row_height = probe.get_row_height();
        let row = (top / row_height).ceil() as i32;
        let source_index = row * probe.get_columns();
        let position = slint::LogicalPosition::new(10., row as f32 * row_height - top + 10.);
        let button = slint::platform::PointerEventButton::Left;
        probe
            .window()
            .dispatch_event(slint::platform::WindowEvent::PointerPressed { position, button });
        probe
            .window()
            .dispatch_event(slint::platform::WindowEvent::PointerReleased { position, button });
        assert_eq!(&*opened.borrow(), &source_index.to_string());
    };
    for top in [0., 3000., 12_000.] {
        probe.set_top(top);
        assert!(probe.get_count() > 0 && probe.get_count() < 40);
        check_action();
    }
    probe
        .window()
        .set_size(slint::LogicalSize::new(1100., 400.));
    assert!(probe.get_count() > 0 && probe.get_count() < 50);
    check_action();
    probe.hide().unwrap();
}
