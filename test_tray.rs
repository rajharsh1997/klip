use ksni::{Tray, MenuItem, blocking::TrayMethods};

struct MyTray;

impl Tray for MyTray {
    fn id(&self) -> String { "klip".into() }
    fn icon_name(&self) -> String { "klipper".into() }
}

fn main() {
    let tray = MyTray;
    let handle = tray.spawn().unwrap();
    std::thread::sleep(std::time::Duration::from_secs(10));
}
