pub mod compose;
#[cfg(debug_assertions)]
mod devscript;
mod html;
mod rows;
mod shortcuts;

use gtk::{gdk, gio, glib, glib::clone, pango, prelude::*};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use webkit6::prelude::*;

use crate::api::{Client, Identities, PendingSender, ThreadDetail, ThreadSummary};
use crate::config::{self, Config};
use crate::theme::{self, Palette};
use crate::util::{self, Draft, unused_path};

const PAGE: u32 = 50;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum View {
    Screener,
    Inbox,
    Archive,
    Sent,
    Search,
}

impl View {
    const NAV: [View; 4] = [View::Screener, View::Inbox, View::Archive, View::Sent];

    fn folder(self) -> &'static str {
        match self {
            View::Screener => "screener",
            View::Inbox | View::Search => "inbox",
            View::Archive => "archive",
            View::Sent => "sent",
        }
    }

    fn title(self) -> &'static str {
        match self {
            View::Screener => "The Screener",
            View::Inbox => "Inbox",
            View::Archive => "Archive",
            View::Sent => "Sent",
            View::Search => "Search",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            View::Screener => "\u{f0b0}",
            View::Inbox => "\u{f01c}",
            View::Archive => "\u{f187}",
            View::Sent => "\u{f1d8}",
            View::Search => "\u{f002}",
        }
    }

    fn nav_index(self) -> Option<usize> {
        Self::NAV.iter().position(|v| *v == self)
    }
}

struct Seen {
    inbox: HashMap<String, i64>,
    screener: HashSet<String>,
}

pub struct Ui {
    pub window: gtk::ApplicationWindow,
    pub client: Option<Client>,
    pub identities: RefCell<Option<Identities>>,
    /// Open compose windows, so closing the main window goes through their discard prompts.
    pub composes: RefCell<Vec<glib::WeakRef<gtk::Window>>>,
    palette: RefCell<Palette>,
    view: Cell<View>,
    before_search: Cell<View>,
    query: RefCell<String>,
    threads: RefCell<Vec<ThreadSummary>>,
    senders: RefCell<Vec<PendingSender>>,
    screener_threads: RefCell<Vec<ThreadSummary>>,
    current: RefCell<Option<ThreadDetail>>,
    /// The thread the user allowed remote images for (L). Tied to one thread, so a press while
    /// another thread is still loading can't unblock that one's trackers.
    images_for: RefCell<Option<(String, HashSet<String>)>>,
    /// The thread an open is in flight for, so a list rebuild keeps that row selected rather
    /// than the one still on screen (which would let e/i/u act on the wrong thread).
    opening: RefCell<Option<String>>,
    list_gen: Cell<u64>,
    /// A full (non-soft) list load is in flight: its rows aren't there yet, so nothing else should
    /// rebuild the list from stale data meanwhile.
    list_loading: Cell<bool>,
    open_gen: Cell<u64>,
    loading_more: Cell<bool>,
    exhausted: Cell<bool>,
    suppress: Cell<bool>,
    seen: RefCell<Option<Seen>>,
    toast_timer: Rc<RefCell<Option<glib::SourceId>>>,
    nav: gtk::ListBox,
    badges: Vec<gtk::Label>,
    wide_only: Vec<gtk::Widget>,
    sidebar: gtk::Box,
    paned: gtk::Paned,
    compact: Cell<Option<bool>>,
    list: gtk::ListBox,
    scroller: gtk::ScrolledWindow,
    empty: gtk::Label,
    list_stack: gtk::Stack,
    title: gtk::Label,
    subtitle: gtk::Label,
    toast: gtk::Label,
    search: gtk::SearchEntry,
    reader_stack: gtk::Stack,
    reader_subject: gtk::Label,
    archive_btn: gtk::Button,
    inbox_btn: gtk::Button,
    webview: webkit6::WebView,
}

fn icon_button(glyph: &str, tooltip: &str) -> gtk::Button {
    let b = gtk::Button::builder().label(glyph).tooltip_text(tooltip).build();
    b.add_css_class("flat");
    b
}

impl Ui {
    pub fn new(app: &gtk::Application, config: Result<Config, String>) -> Rc<Self> {
        let palette = theme::load();
        theme::apply(&palette);

        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .title("Cloudmail")
            .default_width(1320)
            .default_height(840)
            .build();
        window.add_css_class("cloudmail");
        window.set_icon_name(Some(crate::APP_ID));

        // Sidebar
        let sidebar = gtk::Box::new(gtk::Orientation::Vertical, 0);
        sidebar.add_css_class("sidebar");
        sidebar.set_width_request(200);
        let brand = gtk::Label::builder().label("\u{f0e0}  cloudmail").xalign(0.0).build();
        brand.add_css_class("brand");
        sidebar.append(&brand);
        let compose_box = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        compose_box.append(&gtk::Label::new(Some("\u{f040}")));
        let compose_label = gtk::Label::builder().label("Compose").xalign(0.0).build();
        compose_box.append(&compose_label);
        let compose_btn = gtk::Button::builder().child(&compose_box).tooltip_text("New message (c)").build();
        compose_btn.add_css_class("suggested");
        compose_btn.add_css_class("compose");
        sidebar.append(&compose_btn);
        let nav = gtk::ListBox::new();
        nav.set_selection_mode(gtk::SelectionMode::Single);
        let mut badges = Vec::new();
        let mut wide_only: Vec<gtk::Widget> = Vec::new();
        for (i, view) in View::NAV.iter().enumerate() {
            let row_box = gtk::Box::new(gtk::Orientation::Horizontal, 10);
            let icon = gtk::Label::new(Some(view.icon()));
            icon.set_width_chars(2);
            row_box.append(&icon);
            let name = gtk::Label::builder().label(view.title()).xalign(0.0).hexpand(true).build();
            row_box.append(&name);
            wide_only.push(name.upcast());
            let badge = gtk::Label::new(None);
            badge.add_css_class("badge");
            if i != 0 {
                badge.add_css_class("dim");
            }
            badge.set_visible(false);
            row_box.append(&badge);
            badges.push(badge);
            nav.append(&gtk::ListBoxRow::builder().child(&row_box).build());
        }
        sidebar.append(&nav);
        let spacer = gtk::Box::new(gtk::Orientation::Vertical, 0);
        spacer.set_vexpand(true);
        sidebar.append(&spacer);
        let hint = gtk::Label::builder().label("? all shortcuts").xalign(0.0).build();
        hint.add_css_class("hint");
        sidebar.append(&hint);
        wide_only.push(hint.upcast());
        wide_only.push(compose_label.upcast());
        wide_only.push(brand.upcast());

        // Thread list column
        let middle = gtk::Box::new(gtk::Orientation::Vertical, 0);
        middle.set_width_request(260);
        let search = gtk::SearchEntry::builder().placeholder_text("Search all mail   /").build();
        search.add_css_class("search");
        middle.append(&search);
        let title = gtk::Label::builder().xalign(0.0).build();
        title.add_css_class("column-title");
        middle.append(&title);
        let subtitle = gtk::Label::builder().xalign(0.0).ellipsize(pango::EllipsizeMode::End).build();
        subtitle.add_css_class("column-sub");
        middle.append(&subtitle);
        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::Single);
        list.add_css_class("threadlist");
        let empty = gtk::Label::builder().wrap(true).justify(gtk::Justification::Center).build();
        empty.add_css_class("empty");
        empty.set_valign(gtk::Align::Start);
        let scroller = gtk::ScrolledWindow::builder()
            .child(&list)
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();
        scroller.add_css_class("threadlist");
        // ListBox::remove_all() also drops a placeholder, so the empty state is a stack page.
        let list_stack = gtk::Stack::new();
        list_stack.set_vexpand(true);
        list_stack.add_named(&scroller, Some("list"));
        list_stack.add_named(&empty, Some("empty"));
        middle.append(&list_stack);
        let toast = gtk::Label::builder().xalign(0.0).wrap(true).build();
        toast.add_css_class("column-sub");
        toast.set_margin_top(6);
        toast.set_visible(false);
        middle.append(&toast);

        // Reader
        let reader = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        bar.add_css_class("reader-bar");
        // An empty spacer that pushes the buttons right and carries the subject as its tooltip.
        let reader_subject = gtk::Label::builder().hexpand(true).build();
        bar.append(&reader_subject);
        let reply_btn = icon_button("\u{f112}", "Reply (r)");
        let reply_all_btn = icon_button("\u{f122}", "Reply all (a)");
        let archive_btn = icon_button("\u{f187}", "Archive (e)");
        let inbox_btn = icon_button("\u{f01c}", "Move to Inbox (i)");
        let unread_btn = icon_button("\u{f0e0}", "Mark unread (u)");
        let images_btn = icon_button("\u{f03e}", "Load remote images (L)");
        for b in [&reply_btn, &reply_all_btn, &archive_btn, &inbox_btn, &unread_btn, &images_btn] {
            bar.append(b);
        }
        reader.append(&bar);

        let settings = webkit6::Settings::new();
        settings.set_enable_javascript(false);
        settings.set_enable_javascript_markup(false);
        settings.set_enable_developer_extras(false);
        settings.set_enable_back_forward_navigation_gestures(false);
        let webview = webkit6::WebView::builder()
            .network_session(&webkit6::NetworkSession::new_ephemeral())
            .settings(&settings)
            .vexpand(true)
            .hexpand(true)
            .build();
        reader.append(&webview);

        let placeholder = gtk::Label::builder()
            .label("Nothing selected\n\nj / k to move · 1–4 to switch boxes\n? for all keys")
            .justify(gtk::Justification::Center)
            .wrap(true)
            .margin_start(20)
            .margin_end(20)
            .build();
        placeholder.add_css_class("placeholder");
        let reader_stack = gtk::Stack::new();
        reader_stack.add_named(&placeholder, Some("empty"));
        reader_stack.add_named(&reader, Some("thread"));
        reader_stack.set_visible_child_name("empty");

        let paned = gtk::Paned::builder()
            .orientation(gtk::Orientation::Horizontal)
            .start_child(&middle)
            .end_child(&reader_stack)
            .position(430)
            .shrink_start_child(false)
            .resize_start_child(false)
            .hexpand(true)
            .build();

        let root = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        root.append(&sidebar);
        match &config {
            Ok(_) => root.append(&paned),
            Err(msg) => root.append(&setup_page(msg)),
        }
        window.set_child(Some(&root));

        let poll_seconds = config.as_ref().map(|c| c.poll_seconds).unwrap_or(config::DEFAULT_POLL_SECONDS);
        let ui = Rc::new(Self {
            window,
            client: config.as_ref().ok().map(Client::new),
            identities: RefCell::new(None),
            composes: RefCell::new(Vec::new()),
            palette: RefCell::new(palette),
            view: Cell::new(View::Inbox),
            before_search: Cell::new(View::Inbox),
            query: RefCell::new(String::new()),
            threads: RefCell::new(Vec::new()),
            senders: RefCell::new(Vec::new()),
            screener_threads: RefCell::new(Vec::new()),
            current: RefCell::new(None),
            images_for: RefCell::new(None),
            opening: RefCell::new(None),
            list_gen: Cell::new(0),
            list_loading: Cell::new(false),
            open_gen: Cell::new(0),
            loading_more: Cell::new(false),
            exhausted: Cell::new(false),
            suppress: Cell::new(false),
            seen: RefCell::new(None),
            toast_timer: Rc::new(RefCell::new(None)),
            nav,
            badges,
            wide_only,
            sidebar,
            paned,
            compact: Cell::new(None),
            list,
            scroller,
            empty,
            list_stack,
            title,
            subtitle,
            toast,
            search,
            reader_stack,
            reader_subject,
            archive_btn,
            inbox_btn,
            webview,
        });
        ui.apply_webview_background();
        // GTK4 has no resize signal on windows; checking per frame is cheap and
        // catches tiling WM resizes.
        ui.window.add_tick_callback(clone!(
            #[weak] ui,
            #[upgrade_or] glib::ControlFlow::Break,
            move |w, _| {
                ui.apply_width(w.width());
                glib::ControlFlow::Continue
            }
        ));

        ui.nav.connect_row_selected(clone!(#[weak] ui, move |_, row| {
            if ui.suppress.get() {
                return;
            }
            if let Some(view) = row.and_then(|r| View::NAV.get(r.index() as usize))
                && *view != ui.view.get() {
                    ui.set_view(*view);
                }
        }));
        ui.list.connect_row_selected(clone!(#[weak] ui, move |_, row| {
            if ui.suppress.get() {
                return;
            }
            if let Some(row) = row {
                ui.open_index(row.index() as usize);
            }
        }));
        ui.scroller.connect_edge_reached(clone!(#[weak] ui, move |_, pos| {
            if pos == gtk::PositionType::Bottom {
                ui.load_more();
            }
        }));
        ui.search.connect_activate(clone!(#[weak] ui, move |entry| {
            let q = entry.text().trim().to_string();
            if q.is_empty() {
                ui.exit_search();
            } else {
                ui.start_search(q);
            }
        }));

        // Quitting would take open drafts with it. Close each through its own close-request:
        // untouched ones just close, edited ones ask, and the main window waits for them.
        ui.window.connect_close_request(clone!(#[weak] ui, #[upgrade_or] glib::Propagation::Proceed, move |_| {
            let open: Vec<gtk::Window> = ui.composes.borrow().iter().filter_map(|w| w.upgrade()).collect();
            for w in &open {
                w.close();
            }
            let still_open = ui.composes.borrow().iter().filter_map(|w| w.upgrade()).any(|w| w.is_visible());
            if still_open { glib::Propagation::Stop } else { glib::Propagation::Proceed }
        }));
        compose_btn.connect_clicked(clone!(#[weak] ui, move |_| compose::open(&ui, Draft::default())));
        reply_btn.connect_clicked(clone!(#[weak] ui, move |_| ui.reply(false)));
        reply_all_btn.connect_clicked(clone!(#[weak] ui, move |_| ui.reply(true)));
        ui.archive_btn.connect_clicked(clone!(#[weak] ui, move |_| ui.move_current("archive")));
        ui.inbox_btn.connect_clicked(clone!(#[weak] ui, move |_| ui.move_current("inbox")));
        unread_btn.connect_clicked(clone!(#[weak] ui, move |_| ui.toggle_unread()));
        images_btn.connect_clicked(clone!(#[weak] ui, move |_| ui.load_images()));

        ui.webview.connect_decide_policy(clone!(
            #[weak] ui,
            #[upgrade_or] false,
            move |_, decision, kind| ui.decide_policy(decision, kind)
        ));

        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        keys.connect_key_pressed(clone!(
            #[weak] ui,
            #[upgrade_or] glib::Propagation::Proceed,
            move |_, key, _, mods| ui.on_key(key, mods)
        ));
        ui.window.add_controller(keys);

        theme::watch(clone!(#[weak] ui, move |palette| {
            theme::apply(&palette);
            *ui.palette.borrow_mut() = palette;
            ui.apply_webview_background();
            ui.render_current();
        }));

        if ui.client.is_some() {
            ui.set_view(View::Inbox);
            ui.check_new();
            ui.load_identities();
            glib::timeout_add_seconds_local(poll_seconds, clone!(
                #[weak] ui,
                #[upgrade_or] glib::ControlFlow::Break,
                move || {
                    ui.poll();
                    glib::ControlFlow::Continue
                }
            ));
        } else {
            ui.select_nav(View::Inbox);
        }
        #[cfg(debug_assertions)]
        devscript::start(&ui);
        ui
    }

    pub fn present(&self) {
        self.window.present();
    }

    /// Tiled at half a laptop screen the three panes don't fit, so the sidebar
    /// collapses to icons and the list narrows.
    fn apply_width(&self, width: i32) {
        let compact = width > 0 && width < 1000;
        if self.compact.get() == Some(compact) {
            return;
        }
        self.compact.set(Some(compact));
        for w in &self.wide_only {
            w.set_visible(!compact);
        }
        self.sidebar.set_width_request(if compact { 56 } else { 200 });
        self.paned.set_position(if compact { 280 } else { 430 });
    }

    fn apply_webview_background(&self) {
        if let Ok(rgba) = gdk::RGBA::parse(self.palette.borrow().background.as_str()) {
            self.webview.set_background_color(&rgba);
        }
    }

    pub fn toast(&self, msg: &str) {
        self.toast.set_label(msg);
        self.toast.set_visible(true);
        if let Some(id) = self.toast_timer.borrow_mut().take() {
            id.remove();
        }
        let label = self.toast.clone();
        let slot = self.toast_timer.clone();
        let timer = glib::timeout_add_seconds_local_once(5, move || {
            slot.borrow_mut().take();
            label.set_visible(false);
        });
        *self.toast_timer.borrow_mut() = Some(timer);
    }

    // ---- views & lists ---------------------------------------------------

    fn select_nav(&self, view: View) {
        self.suppress.set(true);
        match view.nav_index().and_then(|i| self.nav.row_at_index(i as i32)) {
            Some(row) => self.nav.select_row(Some(&row)),
            None => self.nav.unselect_all(),
        }
        self.suppress.set(false);
    }

    fn set_view(self: &Rc<Self>, view: View) {
        self.view.set(view);
        self.select_nav(view);
        if view != View::Search {
            self.search.set_text("");
        }
        self.title.set_label(&format!("{}  {}", view.icon(), view.title()));
        self.subtitle.set_label("");
        self.clear_reader();
        self.load_list(false);
    }

    fn start_search(self: &Rc<Self>, q: String) {
        if self.view.get() != View::Search {
            self.before_search.set(self.view.get());
        }
        *self.query.borrow_mut() = q;
        self.set_view(View::Search);
        self.list_focus();
    }

    fn exit_search(self: &Rc<Self>) {
        self.search.set_text("");
        if self.view.get() == View::Search {
            self.set_view(self.before_search.get());
        }
        self.list_focus();
    }

    fn list_focus(&self) {
        match self.list.selected_row() {
            Some(row) => row.grab_focus(),
            None => self.list.grab_focus(),
        };
    }

    fn load_list(self: &Rc<Self>, soft: bool) {
        let Some(client) = self.client.clone() else { return };
        let generation = self.list_gen.get() + 1;
        self.list_gen.set(generation);
        self.exhausted.set(false);
        self.loading_more.set(false);
        let view = self.view.get();
        self.list_loading.set(!soft);
        if !soft {
            self.empty.set_label("Loading…");
            self.list_stack.set_visible_child_name("empty");
            self.suppress.set(true);
            self.list.remove_all();
            self.suppress.set(false);
        }
        let query = self.query.borrow().clone();
        util::run(
            move || -> Result<(Vec<ThreadSummary>, Vec<PendingSender>), String> {
                match view {
                    View::Screener => Ok((client.threads("screener", None, None, 200)?, client.screener()?)),
                    View::Search => Ok((client.threads("inbox", Some(&query), None, PAGE)?, Vec::new())),
                    _ => Ok((client.threads(view.folder(), None, None, PAGE)?, Vec::new())),
                }
            },
            clone!(#[weak(rename_to = ui)] self, move |result| {
                if ui.list_gen.get() != generation {
                    return;
                }
                ui.list_loading.set(false);
                match result {
                    Ok((threads, senders)) => {
                        if view == View::Screener {
                            *ui.screener_threads.borrow_mut() = threads;
                            *ui.senders.borrow_mut() = senders;
                        } else {
                            ui.exhausted.set(threads.len() < PAGE as usize);
                            *ui.threads.borrow_mut() = threads;
                        }
                        ui.rebuild_rows();
                    }
                    Err(e) => {
                        if soft {
                            eprintln!("refresh failed: {e}");
                        } else {
                            ui.empty.set_label(&format!("Couldn't load mail\n\n{e}\n\nPress R to retry"));
                            ui.empty.add_css_class("error");
                            ui.sync_empty();
                        }
                    }
                }
            }),
        );
    }

    fn load_more(self: &Rc<Self>) {
        let view = self.view.get();
        if view == View::Screener || self.exhausted.get() || self.loading_more.get() {
            return;
        }
        let Some(client) = self.client.clone() else { return };
        let Some(before) = self.threads.borrow().last().map(|t| t.last_at) else { return };
        self.loading_more.set(true);
        let generation = self.list_gen.get();
        let query = self.query.borrow().clone();
        util::run(
            move || {
                let q = (view == View::Search).then_some(query.as_str());
                client.threads(view.folder(), q, Some(before), PAGE)
            },
            clone!(#[weak(rename_to = ui)] self, move |result: Result<Vec<ThreadSummary>, String>| {
                ui.loading_more.set(false);
                if ui.list_gen.get() != generation {
                    return;
                }
                match result {
                    Ok(more) => {
                        ui.exhausted.set(more.len() < PAGE as usize);
                        let known: HashSet<String> = ui.threads.borrow().iter().map(|t| t.id.clone()).collect();
                        let sent = view == View::Sent;
                        let default_email = ui.default_email();
                        for t in more.into_iter().filter(|t| !known.contains(&t.id)) {
                            ui.list.append(&rows::thread_row(&t, sent, default_email.as_deref()));
                            ui.threads.borrow_mut().push(t);
                        }
                        ui.update_subtitle();
                    }
                    Err(e) => ui.toast(&e),
                }
            }),
        );
    }

    fn rebuild_rows(self: &Rc<Self>) {
        let keep = self.opening.borrow().clone().or_else(|| self.current.borrow().as_ref().map(|d| d.thread.id.clone()));
        let had_focus = self.list.focus_child().is_some();
        self.empty.remove_css_class("error");
        self.suppress.set(true);
        self.list.remove_all();
        let mut select = None;
        if self.view.get() == View::Screener {
            let screener_threads = self.screener_threads.borrow();
            for (i, s) in self.senders.borrow().iter().enumerate() {
                let r = rows::sender_row(s);
                let email = s.email.clone();
                r.approve.connect_clicked(clone!(#[weak(rename_to = ui)] self, #[strong] email, move |_| ui.decide(&email, "approved")));
                r.block.connect_clicked(clone!(#[weak(rename_to = ui)] self, #[strong] email, move |_| ui.decide(&email, "blocked")));
                self.list.append(&r.row);
                let latest = latest_thread_for(&screener_threads, &s.email);
                if keep.is_some() && latest.map(|t| &t.id) == keep.as_ref() {
                    select = Some(i);
                }
            }
            self.empty.set_label("The Screener is empty.\n\nNew senders show up here first.");
        } else {
            let sent = self.view.get() == View::Sent;
            let default_email = self.default_email();
            for (i, t) in self.threads.borrow().iter().enumerate() {
                self.list.append(&rows::thread_row(t, sent, default_email.as_deref()));
                if keep.as_deref() == Some(t.id.as_str()) {
                    select = Some(i);
                }
            }
            self.empty.set_label(match self.view.get() {
                View::Inbox => "Inbox zero.\n\nNothing needs you right now.\nPress c or Compose to write.",
                View::Search => "No matches.",
                _ => "Nothing here.",
            });
        }
        if let Some(row) = select.and_then(|i| self.list.row_at_index(i as i32)) {
            self.list.select_row(Some(&row));
            if had_focus {
                row.grab_focus();
            }
        }
        self.suppress.set(false);
        self.update_subtitle();
    }

    fn sync_empty(&self) {
        let page = if self.list.row_at_index(0).is_some() { "list" } else { "empty" };
        self.list_stack.set_visible_child_name(page);
    }

    fn update_subtitle(&self) {
        let text = match self.view.get() {
            View::Screener => {
                let n = self.senders.borrow().len();
                if n == 0 {
                    String::new()
                } else {
                    format!("{n} new sender{} · y lets them in, n blocks", plural(n))
                }
            }
            View::Search => {
                let n = self.threads.borrow().len();
                format!("“{}” · {n} result{}", self.query.borrow(), plural(n))
            }
            _ => {
                let threads = self.threads.borrow();
                let unread = threads.iter().filter(|t| t.unread).count();
                let more = if self.exhausted.get() { "" } else { "+" };
                let unread = if unread > 0 { format!(" · {unread} unread") } else { String::new() };
                if threads.is_empty() {
                    String::new()
                } else {
                    format!("{}{more} conversation{}{unread}", threads.len(), plural(threads.len()))
                }
            }
        };
        self.subtitle.set_label(&text);
        self.sync_empty();
    }

    // ---- reading ----------------------------------------------------------

    fn clear_reader(&self) {
        self.open_gen.set(self.open_gen.get() + 1);
        *self.current.borrow_mut() = None;
        self.reader_stack.set_visible_child_name("empty");
    }

    fn open_index(self: &Rc<Self>, idx: usize) {
        let id = if self.view.get() == View::Screener {
            let senders = self.senders.borrow();
            let Some(sender) = senders.get(idx) else { return };
            latest_thread_for(&self.screener_threads.borrow(), &sender.email).map(|t| t.id.clone())
        } else {
            self.threads.borrow().get(idx).map(|t| t.id.clone())
        };
        match id {
            Some(id) => self.open_thread(&id, false),
            None => self.clear_reader(),
        }
    }

    fn open_thread(self: &Rc<Self>, id: &str, force: bool) {
        // Every open supersedes any load still in flight, including a quick j-then-k back to the
        // thread already shown; otherwise the late response would replace it under the highlight.
        let generation = self.open_gen.get() + 1;
        self.open_gen.set(generation);
        if !force && self.current.borrow().as_ref().is_some_and(|d| d.thread.id == id) {
            *self.opening.borrow_mut() = None;
            return;
        }
        let Some(client) = self.client.clone() else { return };
        *self.opening.borrow_mut() = Some(id.to_string());
        let id = id.to_string();
        util::run(
            move || client.thread(&id),
            clone!(#[weak(rename_to = ui)] self, move |result: Result<ThreadDetail, String>| {
                if ui.open_gen.get() != generation {
                    return;
                }
                *ui.opening.borrow_mut() = None;
                match result {
                    Ok(detail) => {
                        let unread = detail.thread.unread;
                        let id = detail.thread.id.clone();
                        *ui.current.borrow_mut() = Some(detail);
                        ui.render_current();
                        if unread && ui.view.get() != View::Screener {
                            ui.set_unread(&id, false);
                        }
                    }
                    Err(e) => ui.toast(&format!("Couldn't open conversation: {e}")),
                }
            }),
        );
    }

    fn render_current(&self) {
        let current = self.current.borrow();
        let Some(detail) = current.as_ref() else { return };
        let subject = util::subject_or_placeholder(&detail.thread.subject);
        // The full subject heads the message pane; the toolbar keeps only its buttons.
        self.reader_subject.set_tooltip_text(Some(subject));
        let folder = detail.thread.folder.as_str();
        self.archive_btn.set_visible(folder == "inbox");
        self.inbox_btn.set_visible(folder == "archive");
        // Consent covers the messages that were on screen when L was pressed; moving to another
        // thread ends it, and a message that arrived since stays blocked until L is pressed again.
        if self.images_for.borrow().as_ref().is_some_and(|(id, _)| *id != detail.thread.id) {
            *self.images_for.borrow_mut() = None;
        }
        let images = self
            .images_for
            .borrow()
            .as_ref()
            .is_some_and(|(_, allowed)| detail.messages.iter().all(|m| allowed.contains(&m.id)));
        let html = html::thread(detail, &self.palette.borrow(), images);
        self.webview.load_html(&html, Some("about:blank"));
        self.reader_stack.set_visible_child_name("thread");
    }

    fn load_images(&self) {
        // For the messages on screen, which are what the user is looking at when they press L.
        let consent = self
            .current
            .borrow()
            .as_ref()
            .map(|d| (d.thread.id.clone(), d.messages.iter().map(|m| m.id.clone()).collect::<HashSet<_>>()));
        let Some(consent) = consent else { return };
        if self.images_for.borrow().as_ref() != Some(&consent) {
            *self.images_for.borrow_mut() = Some(consent);
            self.render_current();
        }
    }

    fn decide_policy(self: &Rc<Self>, decision: &webkit6::PolicyDecision, kind: webkit6::PolicyDecisionType) -> bool {
        if !matches!(kind, webkit6::PolicyDecisionType::NavigationAction | webkit6::PolicyDecisionType::NewWindowAction) {
            return false;
        }
        let Some(nav) = decision.downcast_ref::<webkit6::NavigationPolicyDecision>() else { return false };
        let uri = nav
            .navigation_action()
            .and_then(|a| a.request())
            .and_then(|r| r.uri())
            .map(|u| u.to_string())
            .unwrap_or_default();
        if uri.is_empty() || uri.starts_with("about:") {
            return false;
        }
        decision.ignore();
        if let Some(id) = uri.strip_prefix(html::ATTACHMENT_SCHEME) {
            self.download_attachment(id);
        } else if uri.starts_with("mailto:") {
            compose::open(self, util::parse_mailto(&uri));
        } else if uri.starts_with("http://") || uri.starts_with("https://") {
            util::open_uri(&uri);
        }
        true
    }

    fn download_attachment(self: &Rc<Self>, id: &str) {
        let Some(client) = self.client.clone() else { return };
        // Only this thread's own attachments: a link in a message body can't fetch and open
        // some other file by naming its id.
        let Some(filename) = self
            .current
            .borrow()
            .as_ref()
            .and_then(|d| d.messages.iter().flat_map(|m| &m.attachments).find(|a| a.id == id && !a.inline).map(|a| a.filename.clone()))
        else {
            return;
        };
        let id = id.to_string();
        self.toast(&format!("Downloading {filename}…"));
        util::run(
            move || {
                let bytes = client.download_attachment(&id)?.bytes;
                let dir = dirs::download_dir().unwrap_or_else(std::env::temp_dir);
                std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
                let path = unused_path(dir.join(attachment_file_name(&filename)));
                std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
                Ok::<_, String>(path)
            },
            clone!(#[weak(rename_to = ui)] self, move |result: Result<std::path::PathBuf, String>| match result {
                Ok(path) => {
                    ui.toast(&format!("Saved {}", path.display()));
                    util::open_uri(&gio::File::for_path(&path).uri());
                }
                Err(e) => ui.toast(&format!("Download failed: {e}")),
            }),
        );
    }

    // ---- actions ----------------------------------------------------------

    fn selected_index(&self) -> Option<usize> {
        self.list.selected_row().map(|r| r.index() as usize)
    }

    fn move_selection(&self, delta: i32) {
        let count = if self.view.get() == View::Screener { self.senders.borrow().len() } else { self.threads.borrow().len() } as i32;
        if count == 0 {
            return;
        }
        let next = match self.selected_index() {
            Some(i) => (i as i32 + delta).clamp(0, count - 1),
            None => 0,
        };
        if let Some(row) = self.list.row_at_index(next) {
            self.list.select_row(Some(&row));
            row.grab_focus();
        }
    }

    fn remove_row(self: &Rc<Self>, idx: usize) {
        if let Some(row) = self.list.row_at_index(idx as i32) {
            self.suppress.set(true);
            self.list.remove(&row);
            self.suppress.set(false);
        }
        let next = self.list.row_at_index(idx as i32).or_else(|| idx.checked_sub(1).and_then(|i| self.list.row_at_index(i as i32)));
        match next {
            Some(row) => {
                self.list.select_row(Some(&row));
                row.grab_focus();
            }
            None => {
                self.clear_reader();
                self.list.grab_focus();
            }
        }
        self.update_subtitle();
    }

    fn move_current(self: &Rc<Self>, folder: &'static str) {
        let view = self.view.get();
        if view == View::Screener {
            return;
        }
        let Some(client) = self.client.clone() else { return };
        let (idx, id, current_folder) = {
            let threads = self.threads.borrow();
            let idx = self.selected_index().filter(|&i| i < threads.len()).or_else(|| {
                let current = self.current.borrow();
                let current = &current.as_ref()?.thread.id;
                threads.iter().position(|t| t.id == *current)
            });
            let Some(idx) = idx else { return };
            (idx, threads[idx].id.clone(), threads[idx].folder.clone())
        };
        // Only between Inbox and Archive, like the toolbar buttons: a Screener thread (reachable
        // from Search) is moved by screening its sender, never by archiving it.
        if current_folder == folder || !matches!(current_folder.as_str(), "inbox" | "archive") {
            return;
        }
        let leaves_view = matches!((view, folder), (View::Inbox, "archive") | (View::Archive, "inbox"));
        if leaves_view {
            self.threads.borrow_mut().remove(idx);
            self.clear_reader();
            self.remove_row(idx);
        } else {
            self.threads.borrow_mut()[idx].folder = folder.to_string();
            if let Some(d) = self.current.borrow_mut().as_mut().filter(|d| d.thread.id == id) {
                d.thread.folder = folder.to_string();
            }
            self.render_current();
        }
        self.toast(if folder == "archive" { "Archived" } else { "Moved to Inbox" });
        util::run(
            move || client.move_thread(&id, folder),
            clone!(#[weak(rename_to = ui)] self, move |result| {
                if let Err(e) = result {
                    ui.toast(&format!("Couldn't move conversation: {e}"));
                    ui.load_list(true);
                }
                ui.refresh_counts();
            }),
        );
    }

    fn set_unread(self: &Rc<Self>, id: &str, unread: bool) {
        let Some(client) = self.client.clone() else { return };
        // The Screener's rows are senders, so only a thread list has a row to restyle.
        if self.view.get() != View::Screener
            && let Some(i) = self.threads.borrow().iter().position(|t| t.id == id)
            && let Some(row) = self.list.row_at_index(i as i32) {
                rows::set_row_unread(&row, unread);
            }
        for t in self.threads.borrow_mut().iter_mut().filter(|t| t.id == id) {
            t.unread = unread;
        }
        if let Some(d) = self.current.borrow_mut().as_mut().filter(|d| d.thread.id == id) {
            d.thread.unread = unread;
        }
        self.update_subtitle();
        let id = id.to_string();
        util::run(
            move || client.set_unread(&id, unread),
            clone!(#[weak(rename_to = ui)] self, move |result| {
                if let Err(e) = result {
                    ui.toast(&format!("Couldn't update: {e}"));
                }
                ui.refresh_counts();
            }),
        );
    }

    fn toggle_unread(self: &Rc<Self>) {
        // In the Screener the rows are senders, not threads: act on the thread shown.
        let from_row = if self.view.get() == View::Screener {
            None
        } else {
            self.selected_index().and_then(|i| self.threads.borrow().get(i).map(|t| (t.id.clone(), t.unread)))
        };
        let target = from_row.or_else(|| self.current.borrow().as_ref().map(|d| (d.thread.id.clone(), d.thread.unread)));
        if let Some((id, unread)) = target {
            self.set_unread(&id, !unread);
            self.toast(if unread { "Marked read" } else { "Marked unread" });
        }
    }

    fn decide(self: &Rc<Self>, email: &str, status: &'static str) {
        let Some(client) = self.client.clone() else { return };
        let Some(idx) = self.senders.borrow().iter().position(|s| s.email == email) else { return };
        let sender = self.senders.borrow_mut().remove(idx);
        self.screener_threads
            .borrow_mut()
            .retain(|t| !t.is_from(email));
        self.clear_reader();
        self.remove_row(idx);
        self.toast(&format!(
            "{} {}",
            if status == "approved" { "Let in" } else { "Blocked" },
            sender.display()
        ));
        let email = email.to_string();
        util::run(
            move || client.decide_sender(&email, status),
            clone!(#[weak(rename_to = ui)] self, move |result| {
                if let Err(e) = result {
                    ui.toast(&format!("Couldn't screen sender: {e}"));
                    ui.load_list(true);
                }
                ui.refresh_counts();
                if let Some(seen) = ui.seen.borrow_mut().as_mut() {
                    seen.screener.remove(&sender.email.to_ascii_lowercase());
                }
            }),
        );
    }

    fn decide_selected(self: &Rc<Self>, status: &'static str) {
        if self.view.get() != View::Screener {
            return;
        }
        let email = self.selected_index().and_then(|i| self.senders.borrow().get(i).map(|s| s.email.clone()));
        if let Some(email) = email {
            self.decide(&email, status);
        }
    }

    fn default_email(&self) -> Option<String> {
        let ids = self.identities.borrow();
        let ids = ids.as_ref()?;
        ids.default.as_ref().or(ids.identities.first()).map(|a| a.email.clone())
    }

    /// Your mailboxes (lowercased): the addresses the worker lets you send from.
    fn mailboxes(&self) -> HashSet<String> {
        let ids = self.identities.borrow();
        ids.iter().flat_map(|ids| ids.identities.iter().chain(&ids.default)).map(|a| a.email.to_ascii_lowercase()).collect()
    }

    /// Your mailboxes plus any address this conversation shows as yours.
    fn my_addresses(&self) -> HashSet<String> {
        let mut mine = self.mailboxes();
        if let Some(d) = self.current.borrow().as_ref() {
            if let Some(a) = &d.thread.to_address {
                mine.insert(a.to_ascii_lowercase());
            }
            for m in d.messages.iter().filter(|m| m.outgoing) {
                mine.insert(m.from.email.to_ascii_lowercase());
            }
        }
        mine
    }

    fn reply(self: &Rc<Self>, all: bool) {
        let draft = {
            let current = self.current.borrow();
            let Some(detail) = current.as_ref() else { return };
            let Some(msg) = detail.reply_target() else { return };
            let mine = self.my_addresses();
            let primary = msg.reply_recipients();
            let others: Vec<_> = primary.iter().filter(|a| !mine.contains(&a.email.to_ascii_lowercase())).collect();
            let primary = if others.is_empty() { primary.iter().collect() } else { others };
            let mut seen: HashSet<String> = HashSet::new();
            let to: Vec<_> = primary.iter().filter(|a| seen.insert(a.email.to_ascii_lowercase())).map(|a| a.formatted()).collect();
            seen.extend(mine.iter().cloned());
            let cc: Vec<_> = if all {
                msg.to.iter().chain(&msg.cc).filter(|a| seen.insert(a.email.to_ascii_lowercase())).map(|a| a.formatted()).collect()
            } else {
                Vec::new()
            };
            let text = msg.plain_text();
            // Reply from the mailbox the thread came to; an address routed here without a mailbox
            // can't send, so fall back to one of yours the message was sent to, else the default.
            let mailboxes = self.mailboxes();
            let from = detail
                .thread
                .to_address
                .iter()
                .chain(msg.to.iter().chain(&msg.cc).map(|a| &a.email))
                .find(|e| mailboxes.contains(&e.to_ascii_lowercase()))
                .cloned();
            Draft {
                to: to.join(", "),
                cc: cc.join(", "),
                subject: util::reply_subject(&msg.subject),
                body: util::quote(&text, &msg.from.display(), msg.date),
                reply_to_message_id: Some(msg.id.clone()),
                from,
            }
        };
        compose::open(self, draft);
    }

    pub fn after_send(self: &Rc<Self>, thread_id: Option<&str>) {
        self.refresh_counts();
        if let Some(thread_id) = thread_id
            && self.current.borrow().as_ref().is_some_and(|d| d.thread.id == thread_id)
        {
            self.open_thread(thread_id, true);
        }
        if matches!(self.view.get(), View::Sent | View::Inbox) {
            self.load_list(true);
        }
    }

    pub fn load_identities(self: &Rc<Self>) {
        let Some(client) = self.client.clone() else { return };
        util::run(
            move || client.identities(),
            clone!(#[weak(rename_to = ui)] self, move |result| match result {
                Ok(ids) => {
                    *ui.identities.borrow_mut() = Some(ids);
                    // A load still in flight will build its rows with the identities in place.
                    if ui.view.get() != View::Screener && !ui.list_loading.get() {
                        ui.rebuild_rows();
                    }
                }
                Err(e) => eprintln!("identities: {e}"),
            }),
        );
    }

    // ---- polling ----------------------------------------------------------

    fn refresh_counts(self: &Rc<Self>) {
        let Some(client) = self.client.clone() else { return };
        util::run(
            move || client.counts(),
            clone!(#[weak(rename_to = ui)] self, move |result: Result<crate::api::Counts, String>| {
                let Ok(c) = result else { return };
                let set = |i: usize, n: i64| {
                    ui.badges[i].set_label(&n.to_string());
                    ui.badges[i].set_visible(n > 0);
                };
                set(0, c.screener);
                set(1, c.inbox_unread);
                let title = if c.inbox_unread > 0 { format!("Cloudmail ({})", c.inbox_unread) } else { "Cloudmail".into() };
                ui.window.set_title(Some(&title));
            }),
        );
    }

    fn poll(self: &Rc<Self>) {
        let paginated = self.view.get() != View::Screener && self.threads.borrow().len() > PAGE as usize;
        if self.view.get() != View::Search && !paginated {
            self.load_list(true);
        }
        self.check_new();
    }

    fn check_new(self: &Rc<Self>) {
        let Some(client) = self.client.clone() else { return };
        self.refresh_counts();
        util::run(
            move || Ok::<_, cloudmail_api::Error>((client.threads("inbox", None, None, 25)?, client.screener()?)),
            clone!(#[weak(rename_to = ui)] self, move |result: Result<(Vec<ThreadSummary>, Vec<PendingSender>), String>| {
                let Ok((inbox, screener)) = result else { return };
                let fresh = Seen {
                    inbox: inbox.iter().map(|t| (t.id.clone(), t.last_at)).collect(),
                    screener: screener.iter().map(|s| s.email.to_ascii_lowercase()).collect(),
                };
                let mut seen = ui.seen.borrow_mut();
                if let Some(old) = seen.as_ref() {
                    let new_mail: Vec<_> = inbox
                        .iter()
                        .filter(|t| t.unread && old.inbox.get(&t.id).is_none_or(|prev| t.last_at > *prev))
                        .collect();
                    let new_senders: Vec<_> = screener
                        .iter()
                        .filter(|s| !old.screener.contains(&s.email.to_ascii_lowercase()))
                        .collect();
                    if new_mail.len() > 3 {
                        util::notify(&format!("{} new emails", new_mail.len()), "");
                    } else {
                        for t in new_mail {
                            let who = t.sender().unwrap_or_default();
                            util::notify(&who, &t.subject);
                        }
                    }
                    if new_senders.len() > 3 {
                        util::notify("The Screener", &format!("{} new sender{} waiting", new_senders.len(), plural(new_senders.len())));
                    } else {
                        for s in new_senders {
                            util::notify(
                                &format!("Screener: {}", s.display()),
                                s.last_subject.as_deref().unwrap_or(""),
                            );
                        }
                    }
                }
                *seen = Some(fresh);
            }),
        );
    }

    // ---- keyboard ---------------------------------------------------------

    pub(super) fn on_key(self: &Rc<Self>, key: gdk::Key, mods: gdk::ModifierType) -> glib::Propagation {
        use glib::Propagation::{Proceed, Stop};
        let typing = gtk::prelude::GtkWindowExt::focus(&self.window)
            .is_some_and(|w| w.is::<gtk::Text>() || w.is::<gtk::TextView>());
        if key == gdk::Key::Escape {
            if typing || self.view.get() == View::Search {
                self.exit_search();
                return Stop;
            }
            return Proceed;
        }
        if mods.contains(gdk::ModifierType::CONTROL_MASK) {
            if matches!(key, gdk::Key::q | gdk::Key::w) {
                self.window.close();
                return Stop;
            }
            return Proceed;
        }
        if typing || mods.contains(gdk::ModifierType::ALT_MASK) || self.client.is_none() {
            return Proceed;
        }
        if matches!(key, gdk::Key::Return | gdk::Key::KP_Enter) {
            if self.current.borrow().is_some() {
                self.webview.grab_focus();
                return Stop;
            }
            return Proceed;
        }
        let Some(ch) = key.to_unicode() else { return Proceed };
        match ch {
            'j' => self.move_selection(1),
            'k' => self.move_selection(-1),
            'o' => {
                if self.current.borrow().is_some() {
                    self.webview.grab_focus();
                }
            }
            'e' => self.move_current("archive"),
            'i' => self.move_current("inbox"),
            'u' => self.toggle_unread(),
            'r' => self.reply(false),
            'a' => self.reply(true),
            'c' => compose::open(self, Draft::default()),
            '/' => {
                self.search.grab_focus();
            }
            '1'..='4' => self.set_view(View::NAV[(ch as u8 - b'1') as usize]),
            'R' => {
                self.load_list(false);
                self.refresh_counts();
                self.toast("Refreshed");
            }
            'L' => self.load_images(),
            'y' => self.decide_selected("approved"),
            'n' => self.decide_selected("blocked"),
            '?' => shortcuts::show(self.window.upcast_ref()),
            _ => return Proceed,
        }
        Stop
    }
}

fn latest_thread_for<'a>(threads: &'a [ThreadSummary], email: &str) -> Option<&'a ThreadSummary> {
    threads
        .iter()
        .filter(|t| t.is_from(email))
        .max_by_key(|t| t.last_at)
}

/// The sender's filename with path separators and control characters made harmless.
fn attachment_file_name(filename: &str) -> String {
    let clean: String = filename.chars().map(|c| if c == '/' || c.is_control() { '_' } else { c }).collect();
    if clean.trim().is_empty() || clean == "." || clean == ".." { "attachment".to_string() } else { clean }
}

fn setup_page(msg: &str) -> gtk::Widget {
    let b = gtk::Box::new(gtk::Orientation::Vertical, 12);
    b.set_valign(gtk::Align::Center);
    b.set_halign(gtk::Align::Center);
    b.set_hexpand(true);
    b.add_css_class("setup");
    let t = gtk::Label::new(Some("Almost there"));
    t.add_css_class("setup-title");
    b.append(&t);
    let l = gtk::Label::builder().label(msg).selectable(true).wrap(true).xalign(0.0).build();
    b.append(&l);
    b.upcast()
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}
