mod backend;
mod model;
mod ui;

use std::cell::RefCell;
use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;
use gtk4 as gtk;

const APP_ID: &str = "dev.saku.Sink";

fn main() -> glib::ExitCode {
    glib::set_application_name("Sink");

    let app = gtk::Application::builder().application_id(APP_ID).build();

    app.connect_startup(|_| ui::load_css());

    let slot: Rc<RefCell<Option<Rc<RefCell<ui::Ui>>>>> = Rc::new(RefCell::new(None));
    app.connect_activate(move |app| {
        if let Some(window) = app.active_window() {
            window.present();
            return;
        }
        if let Some(window) = app.windows().first() {
            window.present();
            return;
        }
        let ui = ui::create(app);
        *slot.borrow_mut() = Some(ui);
    });

    app.run()
}
