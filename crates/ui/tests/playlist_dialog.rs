use eagle_ui::{Actions, DesktopWindow, Motion, Ui};
use slint::ComponentHandle;
use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Key, Platform, PointerEventButton, WindowAdapter, WindowEvent};
use std::cell::RefCell;
use std::rc::Rc;

struct TestPlatform(Rc<MinimalSoftwareWindow>);
impl Platform for TestPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.0.clone())
    }
}

#[test]
fn playlist_dialog_accepts_spaces_enter_and_escape() {
    slint::platform::set_platform(Box::new(TestPlatform(MinimalSoftwareWindow::new(
        RepaintBufferType::NewBuffer,
    ))))
    .unwrap();
    let window = DesktopWindow::new().unwrap();
    window.global::<Motion>().set_reduced(true);
    window
        .global::<Actions>()
        .on_valid_playlist_name(|name| !name.trim().is_empty());
    let created = Rc::new(RefCell::new(String::new()));
    let result = created.clone();
    window
        .global::<Actions>()
        .on_create_playlist(move |name| *result.borrow_mut() = name.to_string());
    window
        .window()
        .set_size(slint::LogicalSize::new(1280., 880.));
    window.show().unwrap();
    window.global::<Ui>().set_signed_in(true);
    window.global::<Ui>().set_panel_open(true);
    window.global::<Ui>().set_create_playlist(true);
    slint::platform::update_timers_and_animations();
    window.window().take_snapshot().unwrap();
    for text in ["A", " ", "B"] {
        window
            .window()
            .dispatch_event(WindowEvent::KeyPressed { text: text.into() });
    }
    assert_eq!(window.global::<Ui>().get_playlist_name(), "A B");
    for _ in 0..3 {
        window.window().dispatch_event(WindowEvent::KeyPressed {
            text: Key::Tab.into(),
        });
    }
    window
        .window()
        .dispatch_event(WindowEvent::KeyPressed { text: "C".into() });
    assert_eq!(window.global::<Ui>().get_playlist_name(), "A BC");
    for _ in 0..3 {
        window.window().dispatch_event(WindowEvent::KeyPressed {
            text: Key::Backtab.into(),
        });
    }
    window
        .window()
        .dispatch_event(WindowEvent::KeyPressed { text: "D".into() });
    assert_eq!(window.global::<Ui>().get_playlist_name(), "A BCD");
    let before_error = window.window().take_snapshot().unwrap();
    window.global::<Ui>().set_error("Ошибка создания".into());
    let with_error = window.window().take_snapshot().unwrap();
    assert_ne!(before_error.as_bytes(), with_error.as_bytes());
    window
        .window()
        .dispatch_event(WindowEvent::KeyPressed { text: "\n".into() });
    assert_eq!(*created.borrow(), "A BCD");
    window.window().dispatch_event(WindowEvent::KeyPressed {
        text: Key::Escape.into(),
    });
    assert!(!window.global::<Ui>().get_create_playlist());
    assert!(window.global::<Ui>().get_panel_open());
    window
        .window()
        .dispatch_event(WindowEvent::KeyPressed { text: "\n".into() });
    assert!(window.global::<Ui>().get_create_playlist());
    window
        .window()
        .dispatch_event(WindowEvent::KeyPressed { text: "E".into() });
    assert_eq!(window.global::<Ui>().get_playlist_name(), "E");
    window.window().dispatch_event(WindowEvent::KeyPressed {
        text: Key::Tab.into(),
    });
    window
        .window()
        .dispatch_event(WindowEvent::KeyPressed { text: "\n".into() });
    assert!(!window.global::<Ui>().get_create_playlist());
    window
        .window()
        .dispatch_event(WindowEvent::KeyPressed { text: "\n".into() });
    assert!(window.global::<Ui>().get_create_playlist());
    window.window().take_snapshot().unwrap();
    for event in [
        WindowEvent::PointerMoved {
            position: slint::LogicalPosition::new(8., 8.),
        },
        WindowEvent::PointerPressed {
            position: slint::LogicalPosition::new(8., 8.),
            button: PointerEventButton::Left,
        },
        WindowEvent::PointerReleased {
            position: slint::LogicalPosition::new(8., 8.),
            button: PointerEventButton::Left,
        },
    ] {
        window.window().dispatch_event(event);
    }
    assert!(!window.global::<Ui>().get_create_playlist());
    window
        .window()
        .dispatch_event(WindowEvent::KeyPressed { text: "\n".into() });
    assert!(window.global::<Ui>().get_create_playlist());
    window
        .window()
        .set_size(slint::LogicalSize::new(760., 880.));
    window.window().take_snapshot().unwrap();
    window.window().dispatch_event(WindowEvent::KeyPressed {
        text: Key::Escape.into(),
    });
    window
        .window()
        .dispatch_event(WindowEvent::KeyPressed { text: "\n".into() });
    assert!(window.global::<Ui>().get_drawer_open());
}
