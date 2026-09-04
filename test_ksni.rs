use ksni::{Tray, MenuItem};
use glib;

fn main() {
    let (tx, rx) = glib::MainContext::channel::<()>(glib::Priority::DEFAULT);
    let x: glib::Sender<()> = tx;
    let _tray_service = ksni::TrayService::new(MyTray);
}
