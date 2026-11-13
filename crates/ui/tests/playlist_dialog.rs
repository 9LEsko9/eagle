use eagle_ui::{Actions, DesktopWindow, Ui};
use slint::ComponentHandle;
use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Key, Platform, WindowAdapter, WindowEvent};
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
    window
        .window()
        .dispatch_event(WindowEvent::KeyPressed { text: "\n".into() });
    assert_eq!(*created.borrow(), "A B");
    window.window().dispatch_event(WindowEvent::KeyPressed {
        text: Key::Escape.into(),
    });
    assert!(!window.global::<Ui>().get_create_playlist());
    assert!(window.global::<Ui>().get_panel_open());
}
