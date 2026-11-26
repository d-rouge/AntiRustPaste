//! System tray icon via `ksni` (StatusNotifierItem over D-Bus).

use std::sync::mpsc::Sender;

use ksni::blocking::TrayMethods;

#[derive(Clone, Copy, Debug)]
pub enum TrayAction {
    TogglePopup,
    OpenPrefs,
    ExportPdf,
    ClearAll,
    Quit,
}

struct AntiRustPasteTray {
    tx: Sender<TrayAction>,
}

impl ksni::Tray for AntiRustPasteTray {
    fn id(&self) -> String {
        "antirustpaste".to_string()
    }

    fn icon_name(&self) -> String {
        "edit-paste".to_string()
    }

    fn title(&self) -> String {
        "AntiRustPaste".to_string()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: "AntiRustPaste".to_string(),
            description: "Clipboard history".to_string(),
            icon_name: "edit-paste".to_string(),
            icon_pixmap: Vec::new(),
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.tx.send(TrayAction::TogglePopup);
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::StandardItem;
        vec![
            StandardItem {
                label: "Show History".to_string(),
                activate: Box::new(|this: &mut Self| {
                    let _ = this.tx.send(TrayAction::TogglePopup);
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Preferences…".to_string(),
                activate: Box::new(|this: &mut Self| {
                    let _ = this.tx.send(TrayAction::OpenPrefs);
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Export Images to PDF…".to_string(),
                activate: Box::new(|this: &mut Self| {
                    let _ = this.tx.send(TrayAction::ExportPdf);
                }),
                ..Default::default()
            }
            .into(),
            ksni::MenuItem::Separator,
            StandardItem {
                label: "Clear All Clippings".to_string(),
                activate: Box::new(|this: &mut Self| {
                    let _ = this.tx.send(TrayAction::ClearAll);
                }),
                ..Default::default()
            }
            .into(),
            ksni::MenuItem::Separator,
            StandardItem {
                label: "Quit".to_string(),
                activate: Box::new(|this: &mut Self| {
                    let _ = this.tx.send(TrayAction::Quit);
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}

pub struct TrayHandle {
    _handle: ksni::blocking::Handle<AntiRustPasteTray>,
}

pub fn spawn_tray(tx: Sender<TrayAction>) -> Option<TrayHandle> {
    let tray = AntiRustPasteTray { tx };
    match tray.spawn() {
        Ok(handle) => Some(TrayHandle { _handle: handle }),
        Err(err) => {
            eprintln!("antirustpaste-linux: could not register system tray icon: {err}");
            None
        }
    }
}
