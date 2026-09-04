fn main() {
    let (tx, rx) = glib::MainContext::channel::<()>(glib::Priority::DEFAULT);
    let () = tx;
}
