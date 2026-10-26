// GTK4 UI. Plain GTK widgets so the system theme, colors, fonts and dark mode
// are used as-is; no libadwaita, no custom theme.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk::glib;
use gtk::pango;
use gtk::prelude::*;
use gtk4 as gtk;

use crate::backend::{self, ActionSender};
use crate::model::{Action, AppInfo, DeviceInfo, Snapshot};

const CSS: &str = r#"
.section-title {
    font-size: 0.72rem;
    font-weight: 700;
    letter-spacing: 0.09em;
    opacity: 0.55;
}
.dim { opacity: 0.62; }
.small { font-size: 0.85rem; }
.numeric { font-feature-settings: "tnum"; }
.route-item { padding: 5px 9px; }
"#;

#[derive(Clone, Copy)]
enum PageKind {
    Outputs,
    Applications,
    Inputs,
}

type SinkChoices = Rc<RefCell<Vec<(u32, String, String)>>>;

pub struct Ui {
    window: gtk::ApplicationWindow,
    header: gtk::HeaderBar,
    notebook: gtk::Notebook,
    stack: gtk::Stack,
    status_label: gtk::Label,
    retry: gtk::Button,
    output: Page,
    apps: Page,
    inputs: Page,
    sink_rows: HashMap<u32, DeviceRow>,
    source_rows: HashMap<u32, DeviceRow>,
    app_rows: HashMap<u32, AppRow>,
    sink_order: Rc<RefCell<Vec<u32>>>,
    source_order: Rc<RefCell<Vec<u32>>>,
    app_order: Rc<RefCell<Vec<u32>>>,
    sinks: SinkChoices,
    action_tx: Rc<RefCell<Option<ActionSender>>>,
    ready: bool,
}

#[derive(Clone)]
struct Page {
    root: gtk::ScrolledWindow,
    tab_label: gtk::Label,
    section: &'static str,
    list: gtk::ListBox,
    empty: gtk::Label,
    no_matches: Option<gtk::Label>,
    search: Option<gtk::SearchEntry>,
}

impl Page {
    fn set_count(&self, count: usize) {
        self.tab_label
            .set_text(&format!("{} · {count}", self.section));
    }

    fn reset_count(&self) {
        self.tab_label.set_text(self.section);
    }

    fn focus_search(&self) {
        if let Some(search) = &self.search {
            search.grab_focus();
        }
    }

    fn refresh_search(&self) {
        let Some(search) = &self.search else {
            return;
        };
        let query = search.text().trim().to_lowercase();
        let has_rows = self.list.first_child().is_some();
        self.empty.set_visible(!has_rows);

        let Some(no_matches) = &self.no_matches else {
            return;
        };
        if query.is_empty() {
            no_matches.set_visible(false);
            return;
        }

        let mut any_match = false;
        let mut child = self.list.first_child();
        while let Some(widget) = child {
            if let Ok(row) = widget.clone().downcast::<gtk::ListBoxRow>() {
                if row_tooltip_contains(&row, &query) {
                    any_match = true;
                    break;
                }
            }
            child = widget.next_sibling();
        }
        no_matches.set_visible(has_rows && !any_match);
    }
}

struct DeviceRow {
    row: gtk::ListBoxRow,
    check: gtk::Image,
    icon: gtk::Image,
    title: gtk::Label,
    subtitle: gtk::Label,
    volume: VolumeControl,
    is_source: bool,
}

struct AppRow {
    row: gtk::ListBoxRow,
    icon: gtk::Image,
    title: gtk::Label,
    subtitle: gtk::Label,
    route_state: Rc<RouteState>,
    volume: VolumeControl,
}

struct RouteState {
    follow: Cell<bool>,
    sink: Cell<Option<u32>>,
}

struct VolumeControl {
    scale: gtk::Scale,
    percent: gtk::Label,
    mute: gtk::Button,
    mute_icon: gtk::Image,
    updating: Rc<Cell<bool>>,
    last_local: Rc<Cell<Option<Instant>>>,
    muted: Rc<Cell<bool>>,
}

pub fn load_css() {
    register_resources();

    let provider = gtk::CssProvider::new();
    provider.load_from_data(CSS);
    if let Some(display) = gtk::gdk::Display::default() {
        let theme = gtk::IconTheme::for_display(&display);
        theme.add_resource_path("/dev/saku/Sink/icons");
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

fn register_resources() {
    let data = include_bytes!(concat!(env!("OUT_DIR"), "/sink.gresource"));
    let resource = gtk::gio::Resource::from_data(&glib::Bytes::from_static(data))
        .expect("failed to load bundled resources");
    gtk::gio::resources_register(&resource);
}

pub fn create(app: &gtk::Application) -> Rc<RefCell<Ui>> {
    gtk::Window::set_default_icon_name("sink");

    let output = build_page(
        "Outputs",
        "Click a device to make it the system default. Volume and mute apply to the device.",
        "No output devices",
        None,
    );
    let apps = build_page(
        "Applications",
        "Adjust active playback streams or move them to another output.",
        "No active playback streams",
        Some("Search applications"),
    );
    let inputs = build_page(
        "Inputs",
        "Click a device to make it the system default. Volume and mute apply to the device.",
        "No input devices",
        None,
    );

    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("Sink")
        .default_width(720)
        .default_height(560)
        .build();
    window.set_size_request(560, 340);

    let header = gtk::HeaderBar::new();
    let title = gtk::Label::new(Some("Sink"));
    title.add_css_class("title");
    header.set_title_widget(Some(&title));
    window.set_titlebar(Some(&header));

    let notebook = gtk::Notebook::new();
    notebook.set_show_border(false);
    notebook.set_scrollable(true);
    notebook.append_page(&output.root, Some(&output.tab_label));
    notebook.append_page(&apps.root, Some(&apps.tab_label));
    notebook.append_page(&inputs.root, Some(&inputs.tab_label));
    notebook.set_current_page(Some(0));

    let status_label = gtk::Label::new(Some("Connecting to PipeWire…"));
    status_label.set_wrap(true);
    status_label.set_justify(gtk::Justification::Center);
    status_label.set_max_width_chars(42);
    status_label.add_css_class("dim");

    let retry = gtk::Button::with_label("Retry");
    retry.set_halign(gtk::Align::Center);
    retry.set_visible(false);

    let status = gtk::Box::new(gtk::Orientation::Vertical, 10);
    status.set_valign(gtk::Align::Center);
    status.set_halign(gtk::Align::Center);
    status.append(&status_label);
    status.append(&retry);

    let stack = gtk::Stack::new();
    stack.set_transition_type(gtk::StackTransitionType::Crossfade);
    stack.set_transition_duration(120);
    stack.add_named(&status, Some("status"));
    stack.add_named(&notebook, Some("tabs"));
    stack.set_visible_child_name("status");
    window.set_child(Some(&stack));

    let ui = Ui {
        window,
        header,
        notebook,
        stack,
        status_label,
        retry,
        output,
        apps,
        inputs,
        sink_rows: HashMap::new(),
        source_rows: HashMap::new(),
        app_rows: HashMap::new(),
        sink_order: Rc::new(RefCell::new(Vec::new())),
        source_order: Rc::new(RefCell::new(Vec::new())),
        app_order: Rc::new(RefCell::new(Vec::new())),
        sinks: Rc::new(RefCell::new(Vec::new())),
        action_tx: Rc::new(RefCell::new(None)),
        ready: false,
    };
    let ui = Rc::new(RefCell::new(ui));

    wire_row_activation(&ui);
    wire_retry(&ui);
    add_app_actions(app, &ui);
    install_menu(&ui);

    ui.borrow().window.present();
    start_backend(&ui);
    ui
}

fn build_page(
    section: &'static str,
    description: &str,
    empty_text: &str,
    search_placeholder: Option<&str>,
) -> Page {
    let tab_label = gtk::Label::new(Some(section));

    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::None);
    list.set_show_separators(true);
    list.set_activate_on_single_click(true);

    let empty = gtk::Label::new(Some(empty_text));
    empty.add_css_class("dim");
    empty.set_xalign(0.0);
    empty.set_visible(false);

    let description = gtk::Label::new(Some(description));
    description.add_css_class("dim");
    description.add_css_class("small");
    description.set_xalign(0.0);
    description.set_wrap(true);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 10);
    content.set_margin_top(12);
    content.set_margin_bottom(18);
    content.set_margin_start(12);
    content.set_margin_end(12);
    content.append(&description);

    let mut search = None;
    let mut no_matches = None;
    let query = Rc::new(RefCell::new(String::new()));

    if let Some(placeholder) = search_placeholder {
        let entry = gtk::SearchEntry::new();
        entry.set_placeholder_text(Some(placeholder));
        entry.set_hexpand(true);
        entry.set_tooltip_text(Some("Filter this list"));
        content.append(&entry);
        search = Some(entry);

        let label = gtk::Label::new(Some("No matching applications"));
        label.add_css_class("dim");
        label.set_xalign(0.0);
        label.set_visible(false);
        no_matches = Some(label);

        let filter_query = query.clone();
        list.set_filter_func(move |row| {
            let filter_query = filter_query.borrow();
            if filter_query.is_empty() {
                true
            } else {
                row_tooltip_contains(row, &filter_query)
            }
        });
    }

    content.append(&list);
    content.append(&empty);
    if let Some(label) = &no_matches {
        content.append(label);
    }

    let root = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .vexpand(true)
        .child(&content)
        .build();

    let page = Page {
        root,
        tab_label,
        section,
        list,
        empty,
        no_matches,
        search,
    };

    if let Some(entry) = &page.search {
        let page = page.clone();
        let query = query.clone();
        entry.connect_search_changed(move |entry| {
            *query.borrow_mut() = entry.text().trim().to_lowercase();
            page.list.invalidate_filter();
            page.refresh_search();
        });
    }

    page
}

fn wire_row_activation(ui: &Rc<RefCell<Ui>>) {
    {
        let order = ui.borrow().sink_order.clone();
        let action_tx = ui.borrow().action_tx.clone();
        let list = ui.borrow().output.list.clone();
        list.connect_row_activated(move |_, row| {
            let index = row.index();
            if index < 0 {
                return;
            }
            if let Some(id) = order.borrow().get(index as usize) {
                send(&action_tx, Action::SetDefaultSink(*id));
            }
        });
    }
    {
        let order = ui.borrow().source_order.clone();
        let action_tx = ui.borrow().action_tx.clone();
        let list = ui.borrow().inputs.list.clone();
        list.connect_row_activated(move |_, row| {
            let index = row.index();
            if index < 0 {
                return;
            }
            if let Some(id) = order.borrow().get(index as usize) {
                send(&action_tx, Action::SetDefaultSource(*id));
            }
        });
    }
}

fn wire_retry(ui: &Rc<RefCell<Ui>>) {
    let retry = ui.borrow().retry.clone();
    let weak = Rc::downgrade(ui);
    retry.connect_clicked(move |_| {
        if let Some(ui) = weak.upgrade() {
            start_backend(&ui);
        }
    });
}

fn add_app_actions(app: &gtk::Application, ui: &Rc<RefCell<Ui>>) {
    for (name, kind) in [
        ("show-outputs", PageKind::Outputs),
        ("show-apps", PageKind::Applications),
        ("show-inputs", PageKind::Inputs),
    ] {
        let action = gtk::gio::SimpleAction::new(name, None);
        let weak = Rc::downgrade(ui);
        action.connect_activate(move |_, _| {
            if let Some(ui) = weak.upgrade() {
                ui.borrow().present(kind);
            }
        });
        app.add_action(&action);
    }
    app.set_accels_for_action("app.show-outputs", &["<Primary>1"]);
    app.set_accels_for_action("app.show-apps", &["<Primary>2"]);
    app.set_accels_for_action("app.show-inputs", &["<Primary>3"]);

    let search = gtk::gio::SimpleAction::new("search", None);
    let weak = Rc::downgrade(ui);
    search.connect_activate(move |_, _| {
        if let Some(ui) = weak.upgrade() {
            ui.borrow().search_apps();
        }
    });
    app.add_action(&search);
    app.set_accels_for_action("app.search", &["<Primary>f"]);

    let about = gtk::gio::SimpleAction::new("about", None);
    let app_for_about = app.clone();
    about.connect_activate(move |_, _| show_about(&app_for_about));
    app.add_action(&about);
    app.set_accels_for_action("app.about", &["F1"]);

    let quit = gtk::gio::SimpleAction::new("quit-sink", None);
    let app_for_quit = app.clone();
    quit.connect_activate(move |_, _| app_for_quit.quit());
    app.add_action(&quit);
    app.set_accels_for_action("app.quit-sink", &["<Primary>q"]);
}

fn install_menu(ui: &Rc<RefCell<Ui>>) {
    let menu = gtk::gio::Menu::new();

    let search = gtk::gio::Menu::new();
    search.append(Some("Search Applications"), Some("app.search"));
    menu.append_section(None, &search);

    let application = gtk::gio::Menu::new();
    application.append(Some("About Sink"), Some("app.about"));
    application.append(Some("Quit"), Some("app.quit-sink"));
    menu.append_section(None, &application);

    let image = gtk::Image::from_icon_name("sink-menu-symbolic");
    image.set_pixel_size(16);
    let button = gtk::MenuButton::builder().child(&image).build();
    button.set_tooltip_text(Some("Menu"));
    button.set_has_frame(false);
    button.set_menu_model(Some(&menu));
    ui.borrow().header.pack_end(&button);
}

fn show_about(app: &gtk::Application) {
    let dialog = gtk::AboutDialog::builder()
        .program_name("Sink")
        .version(env!("CARGO_PKG_VERSION"))
        .comments("A native PipeWire audio control panel for Wayland.")
        .website("https://github.com/sakuyma/sink")
        .website_label("Project homepage")
        .logo_icon_name("sink")
        .license_type(gtk::License::MitX11)
        .build();
    if let Some(window) = app.active_window() {
        dialog.set_transient_for(Some(&window));
        dialog.set_modal(true);
    }
    dialog.present();
}

fn start_backend(ui: &Rc<RefCell<Ui>>) {
    {
        let mut ui = ui.borrow_mut();
        ui.ready = false;
        ui.set_connecting();
    }

    let (action_tx, snapshots) = backend::start();
    {
        let sender = ui.borrow().action_tx.clone();
        *sender.borrow_mut() = Some(action_tx);
    }

    let weak = Rc::downgrade(ui);
    glib::spawn_future_local(async move {
        while let Ok(snapshot) = snapshots.recv().await {
            if let Some(ui) = weak.upgrade() {
                ui.borrow_mut().apply_snapshot(snapshot);
            }
        }
    });
}

impl Ui {
    fn present(&self, kind: PageKind) {
        let page = match kind {
            PageKind::Outputs => &self.output,
            PageKind::Applications => &self.apps,
            PageKind::Inputs => &self.inputs,
        };
        if let Some(index) = self.notebook.page_num(&page.root) {
            self.notebook.set_current_page(Some(index));
        }
        self.window.present();
    }

    fn search_apps(&self) {
        if let Some(index) = self.notebook.page_num(&self.apps.root) {
            self.notebook.set_current_page(Some(index));
        }
        self.apps.focus_search();
        self.window.present();
    }

    fn set_connecting(&mut self) {
        self.output.reset_count();
        self.apps.reset_count();
        self.inputs.reset_count();
        self.status_label.set_text("Connecting to PipeWire…");
        self.retry.set_visible(false);
        self.stack.set_visible_child_name("status");
    }

    fn set_error(&mut self, error: &str) {
        self.ready = false;
        self.output.reset_count();
        self.apps.reset_count();
        self.inputs.reset_count();
        self.status_label.set_text(error);
        self.retry.set_visible(true);
        self.stack.set_visible_child_name("status");
    }

    fn apply_snapshot(&mut self, snapshot: Snapshot) {
        if let Some(error) = snapshot.error {
            self.set_error(&error);
            return;
        }
        if !snapshot.ready {
            return;
        }
        if !self.ready {
            self.ready = true;
            self.stack.set_visible_child_name("tabs");
        }

        // Snapshot data shared with app route popovers.
        {
            let mut sinks = self.sinks.borrow_mut();
            sinks.clear();
            let mut sorted: Vec<(u32, String, String)> = snapshot
                .sinks
                .iter()
                .map(|device| (device.id, device.title.clone(), device.icon.clone()))
                .collect();
            sorted.sort_by(|a, b| {
                a.1.to_lowercase()
                    .cmp(&b.1.to_lowercase())
                    .then(a.0.cmp(&b.0))
            });
            sinks.extend(sorted);
        }
        let sink_names: HashMap<u32, String> = snapshot
            .sinks
            .iter()
            .map(|device| (device.id, device.title.clone()))
            .collect();

        sync_devices(
            &self.output.list,
            &mut self.sink_rows,
            &self.sink_order,
            &snapshot.sinks,
            &self.action_tx,
            false,
        );
        sync_devices(
            &self.inputs.list,
            &mut self.source_rows,
            &self.source_order,
            &snapshot.sources,
            &self.action_tx,
            true,
        );
        sync_apps(
            &self.apps.list,
            &mut self.app_rows,
            &self.app_order,
            &snapshot.apps,
            &sink_names,
            &self.sinks,
            &self.action_tx,
        );

        self.output.empty.set_visible(snapshot.sinks.is_empty());
        self.inputs.empty.set_visible(snapshot.sources.is_empty());
        self.apps.list.invalidate_filter();
        self.apps.refresh_search();

        self.output.set_count(snapshot.sinks.len());
        self.apps.set_count(snapshot.apps.len());
        self.inputs.set_count(snapshot.sources.len());
    }
}

fn sync_devices(
    list: &gtk::ListBox,
    rows: &mut HashMap<u32, DeviceRow>,
    order: &Rc<RefCell<Vec<u32>>>,
    devices: &[DeviceInfo],
    action_tx: &Rc<RefCell<Option<ActionSender>>>,
    is_source: bool,
) {
    let mut sorted: Vec<&DeviceInfo> = devices.iter().collect();
    sorted.sort_by(|a, b| {
        a.title
            .to_lowercase()
            .cmp(&b.title.to_lowercase())
            .then(a.id.cmp(&b.id))
    });
    let ids: Vec<u32> = sorted.iter().map(|device| device.id).collect();

    if *order.borrow() != ids || rows.len() != ids.len() {
        while let Some(child) = list.first_child() {
            list.remove(&child);
        }
        rows.clear();
        for device in &sorted {
            let row = DeviceRow::new(device, action_tx.clone(), is_source);
            list.append(&row.row);
            rows.insert(device.id, row);
        }
        *order.borrow_mut() = ids;
    } else {
        for device in &sorted {
            if let Some(row) = rows.get_mut(&device.id) {
                row.update(device);
            }
        }
    }
}

fn sync_apps(
    list: &gtk::ListBox,
    rows: &mut HashMap<u32, AppRow>,
    order: &Rc<RefCell<Vec<u32>>>,
    apps: &[AppInfo],
    sink_names: &HashMap<u32, String>,
    sinks: &SinkChoices,
    action_tx: &Rc<RefCell<Option<ActionSender>>>,
) {
    let mut sorted: Vec<&AppInfo> = apps.iter().collect();
    sorted.sort_by(|a, b| {
        a.title
            .to_lowercase()
            .cmp(&b.title.to_lowercase())
            .then(a.id.cmp(&b.id))
    });
    let ids: Vec<u32> = sorted.iter().map(|app| app.id).collect();

    if *order.borrow() != ids || rows.len() != ids.len() {
        while let Some(child) = list.first_child() {
            list.remove(&child);
        }
        rows.clear();
        for app in &sorted {
            let row = AppRow::new(app, sink_names, sinks.clone(), action_tx.clone());
            list.append(&row.row);
            rows.insert(app.id, row);
        }
        *order.borrow_mut() = ids;
    } else {
        for app in &sorted {
            if let Some(row) = rows.get_mut(&app.id) {
                row.update(app, sink_names);
            }
        }
    }
}

impl DeviceRow {
    fn new(
        device: &DeviceInfo,
        action_tx: Rc<RefCell<Option<ActionSender>>>,
        is_source: bool,
    ) -> Self {
        let row = gtk::ListBoxRow::new();
        row.set_activatable(true);
        row.set_focusable(true);
        row.set_cursor_from_name(Some("pointer"));

        let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        hbox.set_margin_top(6);
        hbox.set_margin_bottom(6);
        hbox.set_margin_start(10);
        hbox.set_margin_end(10);

        let check = gtk::Image::from_icon_name("sink-radio-symbolic");
        check.set_pixel_size(16);
        check.set_valign(gtk::Align::Center);

        let icon = gtk::Image::from_icon_name("sink-speaker-symbolic");
        icon.set_pixel_size(20);
        icon.set_valign(gtk::Align::Center);

        let text = gtk::Box::new(gtk::Orientation::Vertical, 0);
        text.set_hexpand(true);
        text.set_valign(gtk::Align::Center);

        let title = gtk::Label::new(None);
        title.set_xalign(0.0);
        title.set_ellipsize(pango::EllipsizeMode::End);
        title.set_single_line_mode(true);

        let subtitle = gtk::Label::new(None);
        subtitle.add_css_class("dim");
        subtitle.add_css_class("small");
        subtitle.set_xalign(0.0);
        subtitle.set_ellipsize(pango::EllipsizeMode::End);
        subtitle.set_single_line_mode(true);
        subtitle.set_visible(false);

        text.append(&title);
        text.append(&subtitle);

        let volume = VolumeControl::new(device.id, device.volume, device.muted, action_tx);

        hbox.append(&check);
        hbox.append(&icon);
        hbox.append(&text);
        hbox.append(&volume.scale);
        hbox.append(&volume.percent);
        hbox.append(&volume.mute);
        row.set_child(Some(&hbox));

        let device_row = Self {
            row,
            check,
            icon,
            title,
            subtitle,
            volume,
            is_source,
        };
        device_row.update(device);
        device_row
    }

    fn update(&self, device: &DeviceInfo) {
        self.title.set_text(&device.title);
        self.subtitle.set_text(&device.subtitle);
        self.subtitle.set_visible(!device.subtitle.is_empty());
        self.icon.set_icon_name(Some(&device_icon(
            &device.icon,
            &device.title,
            self.is_source,
        )));
        self.check.set_icon_name(Some(if device.is_default {
            "sink-radio-checked-symbolic"
        } else {
            "sink-radio-symbolic"
        }));
        self.check
            .set_opacity(if device.is_default { 1.0 } else { 0.45 });

        let status = if device.is_default {
            if self.is_source {
                "Default input"
            } else {
                "Default output"
            }
        } else {
            "Click to make default"
        };
        let mut tooltip = device.title.clone();
        if !device.subtitle.is_empty() {
            tooltip.push_str(" — ");
            tooltip.push_str(&device.subtitle);
        }
        tooltip.push_str(" — ");
        tooltip.push_str(status);
        self.row.set_tooltip_text(Some(&tooltip));

        self.volume.update(device.volume, device.muted);
    }
}

impl AppRow {
    fn new(
        app: &AppInfo,
        sink_names: &HashMap<u32, String>,
        sinks: SinkChoices,
        action_tx: Rc<RefCell<Option<ActionSender>>>,
    ) -> Self {
        let row = gtk::ListBoxRow::new();
        row.set_activatable(false);
        row.set_focusable(false);

        let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        hbox.set_margin_top(6);
        hbox.set_margin_bottom(6);
        hbox.set_margin_start(10);
        hbox.set_margin_end(10);

        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_size_request(16, 1);

        let icon = gtk::Image::from_icon_name("sink-app-symbolic");
        icon.set_pixel_size(20);
        icon.set_valign(gtk::Align::Center);

        let text = gtk::Box::new(gtk::Orientation::Vertical, 0);
        text.set_hexpand(true);
        text.set_valign(gtk::Align::Center);

        let title = gtk::Label::new(None);
        title.set_xalign(0.0);
        title.set_ellipsize(pango::EllipsizeMode::End);
        title.set_single_line_mode(true);

        let subtitle = gtk::Label::new(None);
        subtitle.add_css_class("dim");
        subtitle.add_css_class("small");
        subtitle.set_xalign(0.0);
        subtitle.set_ellipsize(pango::EllipsizeMode::End);
        subtitle.set_single_line_mode(true);

        text.append(&title);
        text.append(&subtitle);

        let route_state = Rc::new(RouteState {
            follow: Cell::new(app.follow_default),
            sink: Cell::new(app.sink_id),
        });
        let route = gtk::MenuButton::new();
        route.set_icon_name("sink-route-symbolic");
        route.set_has_frame(false);
        route.set_tooltip_text(Some("Move to another output"));
        let popover = gtk::Popover::new();
        popover.set_has_arrow(true);
        popover.set_size_request(250, -1);
        let popbox = gtk::Box::new(gtk::Orientation::Vertical, 2);
        popbox.set_margin_top(6);
        popbox.set_margin_bottom(6);
        popbox.set_margin_start(4);
        popbox.set_margin_end(4);
        popover.set_child(Some(&popbox));
        route.set_popover(Some(&popover));

        {
            let popover = popover.clone();
            let popbox = popbox.clone();
            let route_state = route_state.clone();
            let sinks = sinks.clone();
            let action_tx = action_tx.clone();
            let id = app.id;
            route.connect_activate(move |_| {
                fill_route_menu(
                    &popbox,
                    &popover,
                    id,
                    &sinks.borrow(),
                    &route_state,
                    &action_tx,
                );
            });
        }

        let volume = VolumeControl::new(app.id, app.volume, app.muted, action_tx);

        hbox.append(&spacer);
        hbox.append(&icon);
        hbox.append(&text);
        hbox.append(&route);
        hbox.append(&volume.scale);
        hbox.append(&volume.percent);
        hbox.append(&volume.mute);
        row.set_child(Some(&hbox));

        let app_row = Self {
            row,
            icon,
            title,
            subtitle,
            route_state,
            volume,
        };
        app_row.update(app, sink_names);
        app_row
    }

    fn update(&self, app: &AppInfo, sink_names: &HashMap<u32, String>) {
        self.title.set_text(&app.title);
        self.icon.set_icon_name(Some(&app_icon(&app.icon)));
        self.route_state.follow.set(app.follow_default);
        self.route_state.sink.set(app.sink_id);
        let sink = app
            .sink_id
            .and_then(|id| sink_names.get(&id))
            .map_or("No output", String::as_str);
        let route = if app.follow_default {
            format!("Following default → {sink}")
        } else {
            format!("Pinned → {sink}")
        };
        self.subtitle.set_text(&route);
        self.row
            .set_tooltip_text(Some(&format!("{} — {route}", app.title)));
        self.volume.update(app.volume, app.muted);
    }
}

impl VolumeControl {
    fn new(
        id: u32,
        volume: f32,
        muted: bool,
        action_tx: Rc<RefCell<Option<ActionSender>>>,
    ) -> Self {
        let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 100.0, 1.0);
        scale.set_draw_value(false);
        scale.set_hexpand(false);
        scale.set_size_request(108, -1);
        scale.set_valign(gtk::Align::Center);
        scale.set_increments(1.0, 10.0);
        scale.set_tooltip_text(Some("Volume"));

        let percent = gtk::Label::new(Some("100%"));
        percent.add_css_class("dim");
        percent.add_css_class("numeric");
        percent.set_width_chars(4);
        percent.set_xalign(1.0);
        percent.set_valign(gtk::Align::Center);

        let mute_icon = gtk::Image::from_icon_name("sink-volume-high-symbolic");
        mute_icon.set_pixel_size(16);
        let mute = gtk::Button::builder().child(&mute_icon).build();
        mute.set_has_frame(false);
        mute.set_valign(gtk::Align::Center);
        mute.set_tooltip_text(Some("Mute"));

        let updating = Rc::new(Cell::new(true));
        let last_local: Rc<Cell<Option<Instant>>> = Rc::new(Cell::new(None));
        let muted_cell = Rc::new(Cell::new(muted));

        scale.set_value(volume as f64);
        updating.set(false);
        percent.set_text(&format!("{volume:.0}%"));

        {
            let updating = updating.clone();
            let last_local = last_local.clone();
            let action_tx = action_tx.clone();
            let percent_label = percent.clone();
            scale.connect_value_changed(move |scale| {
                if updating.get() {
                    return;
                }
                let value = scale.value();
                percent_label.set_text(&format!("{value:.0}%"));
                last_local.set(Some(Instant::now()));
                if let Some(tx) = action_tx.borrow().as_ref() {
                    let _ = tx.send(Action::SetVolume {
                        id,
                        percent: value as f32,
                    });
                }
            });
        }

        {
            let muted = muted_cell.clone();
            let action_tx = action_tx.clone();
            let mute_icon = mute_icon.clone();
            let scale = scale.clone();
            mute.connect_clicked(move |_| {
                let next = !muted.get();
                muted.set(next);
                mute_icon.set_icon_name(Some(volume_icon(scale.value() as f32, next)));
                if let Some(tx) = action_tx.borrow().as_ref() {
                    let _ = tx.send(Action::SetMute { id, muted: next });
                }
            });
        }

        Self {
            scale,
            percent,
            mute,
            mute_icon,
            updating,
            last_local,
            muted: muted_cell,
        }
    }

    fn update(&self, volume: f32, muted: bool) {
        let recent = self
            .last_local
            .get()
            .is_some_and(|time| time.elapsed() < Duration::from_millis(400));
        if !recent {
            self.updating.set(true);
            if (self.scale.value() - volume as f64).abs() > 0.15 {
                self.scale.set_value(volume as f64);
            }
            self.updating.set(false);
            self.percent.set_text(&format!("{volume:.0}%"));
        }
        self.muted.set(muted);
        self.mute_icon
            .set_icon_name(Some(volume_icon(volume, muted)));
        self.mute
            .set_tooltip_text(Some(if muted { "Unmute" } else { "Mute" }));
    }
}

fn fill_route_menu(
    popbox: &gtk::Box,
    popover: &gtk::Popover,
    stream_id: u32,
    sinks: &[(u32, String, String)],
    route: &Rc<RouteState>,
    action_tx: &Rc<RefCell<Option<ActionSender>>>,
) {
    while let Some(child) = popbox.first_child() {
        popbox.remove(&child);
    }

    let heading = gtk::Label::new(Some("MOVE TO"));
    heading.add_css_class("section-title");
    heading.set_xalign(0.0);
    heading.set_margin_start(6);
    heading.set_margin_end(6);
    popbox.append(&heading);

    let follow_active = route.follow.get();
    let follow = route_item("sink-home-symbolic", "Follow system default", follow_active);
    {
        let popover = popover.clone();
        let route = route.clone();
        let action_tx = action_tx.clone();
        follow.connect_clicked(move |_| {
            route.follow.set(true);
            route.sink.set(None);
            send(
                &action_tx,
                Action::MoveStream {
                    id: stream_id,
                    sink_id: None,
                },
            );
            popover.popdown();
        });
    }
    popbox.append(&follow);

    if !sinks.is_empty() {
        popbox.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    }
    for (id, title, icon) in sinks {
        let active = !follow_active && route.sink.get() == Some(*id);
        let item = route_item(&device_icon(icon, title, false), title, active);
        let sink_id = *id;
        let popover = popover.clone();
        let route = route.clone();
        let action_tx = action_tx.clone();
        item.connect_clicked(move |_| {
            route.follow.set(false);
            route.sink.set(Some(sink_id));
            send(
                &action_tx,
                Action::MoveStream {
                    id: stream_id,
                    sink_id: Some(sink_id),
                },
            );
            popover.popdown();
        });
        popbox.append(&item);
    }
}

fn route_item(icon: &str, label: &str, active: bool) -> gtk::Button {
    let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let image = gtk::Image::from_icon_name(icon);
    image.set_pixel_size(16);
    image.set_valign(gtk::Align::Center);

    let text = gtk::Label::new(Some(label));
    text.set_xalign(0.0);
    text.set_hexpand(true);
    text.set_ellipsize(pango::EllipsizeMode::End);

    let check = gtk::Image::from_icon_name("sink-check-symbolic");
    check.set_pixel_size(16);
    check.set_visible(active);

    hbox.append(&image);
    hbox.append(&text);
    hbox.append(&check);

    let button = gtk::Button::builder().child(&hbox).build();
    button.set_has_frame(false);
    button.set_halign(gtk::Align::Fill);
    button.add_css_class("route-item");
    button
}

fn send(action_tx: &Rc<RefCell<Option<ActionSender>>>, action: Action) {
    if let Some(tx) = action_tx.borrow().as_ref() {
        let _ = tx.send(action);
    }
}

fn row_tooltip_contains(row: &gtk::ListBoxRow, query: &str) -> bool {
    row.tooltip_text()
        .map(|text| text.to_lowercase().contains(query))
        .unwrap_or(false)
}

fn volume_icon(volume: f32, muted: bool) -> &'static str {
    if muted || volume <= 0.0 {
        "sink-volume-muted-symbolic"
    } else if volume >= 66.0 {
        "sink-volume-high-symbolic"
    } else if volume >= 33.0 {
        "sink-volume-medium-symbolic"
    } else {
        "sink-volume-low-symbolic"
    }
}

fn device_icon(raw: &str, title: &str, is_source: bool) -> String {
    let haystack = format!("{raw} {title}").to_lowercase();
    if is_source {
        if haystack.contains("webcam")
            || haystack.contains("camera")
            || haystack.contains("hd 2300")
        {
            "sink-webcam-symbolic".to_string()
        } else {
            "sink-mic-symbolic".to_string()
        }
    } else if haystack.contains("headset") || haystack.contains("headphone") {
        "sink-headphones-symbolic".to_string()
    } else if haystack.contains("hdmi") || haystack.contains("display") {
        "sink-display-symbolic".to_string()
    } else {
        "sink-speaker-symbolic".to_string()
    }
}

fn app_icon(name: &str) -> String {
    if name.is_empty() {
        return "sink-app-symbolic".to_string();
    }
    let Some(display) = gtk::gdk::Display::default() else {
        return "sink-app-symbolic".to_string();
    };
    let theme = gtk::IconTheme::for_display(&display);
    if theme.has_icon(name) {
        name.to_string()
    } else {
        "sink-app-symbolic".to_string()
    }
}
