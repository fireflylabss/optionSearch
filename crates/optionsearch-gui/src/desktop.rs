//! The optionSearch desktop window: type-ahead search over the local index with an
//! instant text / image / PDF preview pane.
//!
//! The layout follows GNOME/Adwaita patterns and the system theme:
//! search lives in the header bar, results are rich `AdwActionRow` lines,
//! the detail pane is a card with a read-only preview, and index controls
//! live in the sidebar footer Files/Nautilus-style (buttons + live status),
//! with toast feedback. The header menu is only an overflow (shortcuts/about).
//!
//! Index work (full reindex, add-folder scan) runs on a background thread so
//! typing stays instant; the main loop only flips widgets back on completion.
//! There is intentionally NO cancel button: `Engine::reindex` / `add_root`
//! run one synchronous `scan()` with no interruption hook, so cancelling
//! would mean either killing the thread mid-write (unsafe: it holds the
//! index write lock and the DB lock) or adding a cancellation flag to the
//! engine/scan code, which is out of scope for this window. The scan itself
//! is the slow part and it runs off-thread, so the window never freezes.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use adw::prelude::*;
use anyhow::Result;
use optionsearch_core::{Config, Engine, Preview, PreviewLimits, Watcher};

const APP_ID: &str = "io.option.search";

type Refresh = Rc<dyn Fn(&str)>;

thread_local! {
    /// Process-wide engine watcher, kept alive for the app's lifetime —
    /// dropping a `Watcher` stops its worker thread. The window and daemon
    /// mode share this single instance.
    static ENGINE_WATCHER: RefCell<Option<Watcher>> = const { RefCell::new(None) };
}

/// Starts the inotify watcher once per process; later calls are no-ops.
fn ensure_engine_watcher(index: &Arc<Engine>) {
    ENGINE_WATCHER.with(|slot| {
        if slot.borrow().is_none() {
            *slot.borrow_mut() = Watcher::start_with(
                index.clone(),
                optionsearch_core::WatchOptions::from_config(&index.config()),
            )
            .ok();
        }
    });
}

pub fn run() -> Result<()> {
    let app: adw::Application = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(activate_or_build);
    app.run();
    Ok(())
}

/// `--daemon`: hold the application with no window while the inotify watcher
/// keeps the index current in the background. Because the app id is
/// single-instance, a later plain `optionsearch-gtk` activates this process —
/// `command-line` sees the remote argv, so `--daemon` stays headless while a
/// bare launch opens the window on top of the already-live index.
pub fn run_daemon() -> Result<()> {
    let index = Arc::new(open_engine()?);
    let app: adw::Application = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();
    app.connect_command_line({
        let index = index.clone();
        let started = Rc::new(Cell::new(false));
        // The hold guard releases on drop; keep it parked for the process.
        let hold = Rc::new(RefCell::new(None::<gio::ApplicationHoldGuard>));
        move |app, cmdline| {
            let daemon = cmdline
                .arguments()
                .iter()
                .skip(1)
                .any(|a| a.to_str() == Some("--daemon") || a.to_str() == Some("daemon"));
            if daemon {
                if !started.replace(true) {
                    *hold.borrow_mut() = Some(app.hold());
                    ensure_engine_watcher(&index);
                }
                cmdline.print_literal("optionsearch-gtk: watching index in the background\n");
            } else {
                activate_or_build(app);
            }
            0
        }
    });
    // Remote launches without HANDLES_COMMAND_LINE (a plain `optionsearch-gtk`)
    // arrive as `activate`, not `command-line` — both paths open the window.
    app.connect_activate(activate_or_build);
    app.run();
    Ok(())
}

/// Re-activation presents the existing window instead of stacking a new one.
fn activate_or_build(app: &adw::Application) {
    if let Some(window) = app.windows().first() {
        window.present();
    } else {
        build(app);
    }
}

fn open_engine() -> Result<Engine> {
    let app_sdk = optionsearch_cli::commands::prepare_state()?;
    let config = Config {
        db_path: app_sdk.path("index.sqlite3"),
        cache_dir: app_sdk.path("cache"),
        ..Config::default()
    };
    Engine::open(config)
}

fn build(app: &adw::Application) {
    let index = match open_engine() {
        Ok(index) => Arc::new(index),
        Err(error) => {
            eprintln!("optionsearch-gtk: {error}");
            return;
        }
    };

    if let Some(display) = gtk::gdk::Display::default() {
        install_css(&display);
    }

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("optionSearch")
        .default_width(1120)
        .default_height(720)
        .build();
    window.add_css_class("optionsearch-opaque");

    // Header: search centered like GNOME apps; the menu is overflow only
    // (index controls live in the sidebar footer, not duplicated here).
    let search = gtk::SearchEntry::builder()
        .placeholder_text("Search files…")
        .tooltip_text("Search indexed files (Ctrl+F)")
        .hexpand(true)
        .width_request(360)
        .build();

    let overflow = gio::Menu::new();
    overflow.append(Some("Keyboard shortcuts"), Some("win.shortcuts"));
    overflow.append(Some("About optionSearch"), Some("win.about"));
    let menu_button = gtk::MenuButton::new();
    menu_button.set_icon_name("open-menu-symbolic");
    menu_button.set_tooltip_text(Some("More"));
    menu_button.set_menu_model(Some(&overflow));

    let header = adw::HeaderBar::new();
    // Window controls live inside the sidebar's own header; this bar
    // shows none so they aren't duplicated.
    header.set_show_start_title_buttons(false);
    header.set_show_end_title_buttons(false);
    header.set_title_widget(Some(&search));
    header.pack_end(&menu_button);

    // Sidebar: result count + rich rows, or an empty-state status page.
    let count = gtk::Label::new(None);
    count.set_xalign(0.0);
    count.add_css_class("dim-label");
    count.set_margin_start(4);

    let results = gtk::ListBox::new();
    results.add_css_class("boxed-list");
    results.set_selection_mode(gtk::SelectionMode::Single);
    results.set_activate_on_single_click(true);
    results.set_tooltip_text(Some(
        "Enter previews · Ctrl+Enter opens · Right-click for more actions",
    ));

    let results_scroll = gtk::ScrolledWindow::builder()
        .child(&results)
        .hexpand(true)
        .vexpand(true)
        .build();

    let results_page = gtk::Box::new(gtk::Orientation::Vertical, 8);
    results_page.set_margin_start(12);
    results_page.set_margin_end(12);
    results_page.set_margin_top(12);
    results_page.set_margin_bottom(12);
    results_page.append(&count);
    results_page.append(&results_scroll);

    let empty_page = adw::StatusPage::builder()
        .icon_name("system-search-symbolic")
        .title("Start searching")
        .description("Type above to search every indexed file. Everything stays on this machine.")
        .vexpand(true)
        .build();

    let sidebar_stack = gtk::Stack::new();
    sidebar_stack.add_named(&empty_page, Some("empty"));
    sidebar_stack.add_named(&results_page, Some("results"));
    sidebar_stack.set_vexpand(true);
    sidebar_stack.set_hexpand(true);

    // Sidebar footer: index controls Files/Nautilus-style — buttons + state.
    let index_heading = gtk::Label::new(Some("Index"));
    index_heading.set_xalign(0.0);
    index_heading.add_css_class("heading");

    let spinner = gtk::Spinner::new();
    spinner.set_tooltip_text(Some("Working on the index…"));
    spinner.set_visible(false);

    let heading_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    heading_row.append(&index_heading);
    let head_spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    head_spacer.set_hexpand(true);
    heading_row.append(&head_spacer);
    heading_row.append(&spinner);

    let status = gtk::Label::new(None);
    status.set_xalign(0.0);
    status.set_wrap(true);
    status.add_css_class("dim-label");

    let progress_label = gtk::Label::new(None);
    progress_label.set_xalign(0.0);
    progress_label.add_css_class("dim-label");
    progress_label.set_visible(false);

    // The engine reports no live file count mid-scan, so this pulses.
    let progress = gtk::ProgressBar::new();
    progress.set_show_text(false);
    progress.set_visible(false);

    let build_btn = gtk::Button::with_label("Build index");
    build_btn.set_tooltip_text(Some("Rebuild the whole index in the background"));
    build_btn.set_hexpand(true);
    let add_btn = gtk::Button::with_label("Add folder");
    add_btn.set_tooltip_text(Some("Add a folder to the index"));
    add_btn.set_hexpand(true);
    let clear_btn = gtk::Button::with_label("Clear");
    clear_btn.set_tooltip_text(Some("Clear the search field and preview"));
    let buttons_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    buttons_row.set_homogeneous(true);
    buttons_row.append(&build_btn);
    buttons_row.append(&add_btn);
    buttons_row.append(&clear_btn);

    let controls = gtk::Box::new(gtk::Orientation::Vertical, 6);
    controls.set_margin_start(12);
    controls.set_margin_end(12);
    controls.set_margin_top(8);
    controls.set_margin_bottom(12);
    controls.append(&heading_row);
    controls.append(&status);
    controls.append(&progress_label);
    controls.append(&progress);
    controls.append(&buttons_row);

    // Sidebar top bar: the window's title buttons live here, following
    // `gtk-decoration-layout` like the other Option GTK apps. The title
    // widget keeps the header from echoing the full window title.
    let side_head = adw::HeaderBar::new();
    side_head.add_css_class("flat");
    side_head.set_title_widget(Some(&adw::WindowTitle::new("Search", "")));
    side_head.set_show_start_title_buttons(true);
    side_head.set_show_end_title_buttons(true);

    let sidebar_column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    sidebar_column.add_css_class("sidebar-column");
    sidebar_column.append(&side_head);
    sidebar_column.append(&sidebar_stack);
    sidebar_column.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    sidebar_column.append(&controls);

    // Detail: empty status vs. preview card (header + read-only body).
    let detail_empty = adw::StatusPage::builder()
        .icon_name("document-open-symbolic")
        .title("Select a file")
        .description("Pick a result to preview text, images and PDFs.")
        .vexpand(true)
        .build();

    let detail_icon = gtk::Image::new();
    detail_icon.set_pixel_size(40);
    let preview_title = gtk::Label::new(None);
    preview_title.set_xalign(0.0);
    preview_title.add_css_class("title-2");
    preview_title.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    let preview_meta = gtk::Label::new(None);
    preview_meta.set_xalign(0.0);
    preview_meta.set_wrap(true);
    preview_meta.set_ellipsize(gtk::pango::EllipsizeMode::End);
    preview_meta.add_css_class("dim-label");
    let detail_text = gtk::Box::new(gtk::Orientation::Vertical, 2);
    detail_text.set_hexpand(true);
    detail_text.append(&preview_title);
    detail_text.append(&preview_meta);
    let detail_head = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    detail_head.append(&detail_icon);
    detail_head.append(&detail_text);

    let text = gtk::TextView::new();
    text.set_editable(false);
    text.set_cursor_visible(false);
    text.set_monospace(true);
    text.set_wrap_mode(gtk::WrapMode::WordChar);
    text.set_left_margin(12);
    text.set_right_margin(12);
    text.set_top_margin(12);
    text.set_bottom_margin(12);
    let text_scroll = gtk::ScrolledWindow::builder()
        .child(&text)
        .hexpand(true)
        .vexpand(true)
        .build();
    text_scroll.add_css_class("card");

    let picture = gtk::Picture::new();
    picture.set_can_shrink(true);
    picture.set_content_fit(gtk::ContentFit::Contain);
    picture.set_hexpand(true);
    picture.set_vexpand(true);

    // Audio page: icon plus persistent play/seek controls backed by a
    // MediaFile stream; the tick below keeps icon, scale and time in sync.
    let audio_icon = gtk::Image::from_icon_name("audio-x-generic-symbolic");
    audio_icon.set_pixel_size(72);
    audio_icon.add_css_class("dim-label");

    let play_btn = gtk::Button::from_icon_name("media-playback-start-symbolic");
    play_btn.set_tooltip_text(Some("Play / pause"));
    play_btn.add_css_class("circular");
    play_btn.add_css_class("suggested-action");
    play_btn.set_sensitive(false);

    let seek = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 0.1);
    seek.set_hexpand(true);
    seek.set_draw_value(false);
    seek.set_sensitive(false);

    let time = gtk::Label::new(Some("0:00 / 0:00"));
    time.add_css_class("dim-label");
    time.add_css_class("numeric");

    let audio_controls = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    audio_controls.set_valign(gtk::Align::Center);
    audio_controls.append(&play_btn);
    audio_controls.append(&seek);
    audio_controls.append(&time);

    let audio_page = gtk::Box::new(gtk::Orientation::Vertical, 24);
    audio_page.set_valign(gtk::Align::Center);
    audio_page.set_margin_start(24);
    audio_page.set_margin_end(24);
    audio_page.set_margin_top(24);
    audio_page.set_margin_bottom(24);
    audio_page.append(&audio_icon);
    audio_page.append(&audio_controls);

    let preview_stack = gtk::Stack::new();
    preview_stack.add_named(&text_scroll, Some("text"));
    preview_stack.add_named(&picture, Some("image"));
    preview_stack.add_named(&audio_page, Some("audio"));
    preview_stack.set_vexpand(true);
    preview_stack.set_hexpand(true);

    let separator = gtk::Separator::new(gtk::Orientation::Horizontal);

    let preview_page = gtk::Box::new(gtk::Orientation::Vertical, 12);
    preview_page.set_margin_start(18);
    preview_page.set_margin_end(18);
    preview_page.set_margin_top(18);
    preview_page.set_margin_bottom(18);
    preview_page.append(&detail_head);
    preview_page.append(&separator);
    preview_page.append(&preview_stack);

    let detail_stack = gtk::Stack::new();
    detail_stack.add_named(&detail_empty, Some("empty"));
    detail_stack.add_named(&preview_page, Some("preview"));
    detail_stack.set_vexpand(true);
    detail_stack.set_hexpand(true);

    // Content column: the search header sits beside the sidebar, over the
    // detail pane only — not above the list.
    let content_column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content_column.add_css_class("detail-pane");
    content_column.append(&header);
    content_column.append(&detail_stack);

    // Responsive list ↔ detail: the detail collapses over/under the list
    // on narrow windows, following the system layout.
    let split = adw::OverlaySplitView::builder()
        .sidebar(&sidebar_column)
        .content(&content_column)
        .show_sidebar(true)
        .build();
    split.set_sidebar_width_fraction(0.36);
    split.set_min_sidebar_width(300.0);
    split.set_max_sidebar_width(480.0);
    split.set_collapsed(false);

    // Window controls ride in the sidebar header — but when a narrow window
    // hides the sidebar over the content, they move to the content header
    // so close/minimize/maximize stay reachable.
    let sync_title_buttons = {
        let header = header.clone();
        let side_head = side_head.clone();
        let split = split.clone();
        move || {
            let in_sidebar = !split.is_collapsed() || split.shows_sidebar();
            side_head.set_show_start_title_buttons(in_sidebar);
            side_head.set_show_end_title_buttons(in_sidebar);
            header.set_show_start_title_buttons(!in_sidebar);
            header.set_show_end_title_buttons(!in_sidebar);
        }
    };
    split.connect_collapsed_notify({
        let sync = sync_title_buttons.clone();
        move |_| sync()
    });
    split.connect_show_sidebar_notify({
        let sync = sync_title_buttons.clone();
        move |_| sync()
    });
    sync_title_buttons();

    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&split));
    window.set_content(Some(&toasts));

    let ui = Rc::new(Ui {
        results: results.clone(),
        paths: RefCell::new(Vec::new()),
        sidebar_stack: sidebar_stack.clone(),
        empty_page: empty_page.clone(),
        count: count.clone(),
        detail_stack: detail_stack.clone(),
        icon: detail_icon.clone(),
        title: preview_title.clone(),
        meta: preview_meta.clone(),
        text: text.clone(),
        stack: preview_stack.clone(),
        picture: picture.clone(),
        media: RefCell::new(None),
        play_btn: play_btn.clone(),
        seek: seek.clone(),
        time: time.clone(),
        seeking: Cell::new(false),
        search: search.clone(),
        split: split.clone(),
        toasts: toasts.clone(),
        spinner: spinner.clone(),
        status: status.clone(),
        progress: progress.clone(),
        progress_label: progress_label.clone(),
        build_btn: build_btn.clone(),
        add_btn: add_btn.clone(),
        busy: Cell::new(false),
    });
    ui.status.set_text(&stats_text(&index));

    let refresh: Refresh = Rc::new({
        let ui = ui.clone();
        let index = index.clone();
        move |query: &str| {
            // Reads the current index under a short read lock, so this stays
            // instant even while a background thread is scanning: the scan
            // builds a fresh index off-thread and only swaps it in at the end.
            let found = index.search(query, 100).hits;
            ui.render(&found, query);
        }
    });
    refresh("");

    // Live search.
    search.connect_search_changed({
        let refresh = refresh.clone();
        let split = split.clone();
        move |entry| {
            refresh(&entry.text());
            split.set_show_sidebar(true);
        }
    });
    // Enter previews the first hit.
    search.connect_activate({
        let ui = ui.clone();
        move |_| ui.preview_first()
    });
    // Clicking / activating a row previews it.
    results.connect_row_activated({
        let ui = ui.clone();
        move |_, row| ui.preview_row(row.index())
    });

    // Right-click opens the per-file menu; double-click opens the file.
    let right_click = gtk::GestureClick::builder().button(3).build();
    right_click.connect_pressed({
        let ui = ui.clone();
        let results = results.clone();
        move |_, _, x, y| {
            if let Some(row) = results.row_at_y(y as i32) {
                results.select_row(Some(&row));
                if let Some(path) = ui.path_at(row.index()) {
                    ui.popup_for_path(&path, &row);
                }
            }
            let _ = x;
        }
    });
    results.add_controller(right_click);

    let double_click = gtk::GestureClick::builder().button(1).build();
    double_click.connect_pressed({
        let ui = ui.clone();
        let results = results.clone();
        move |_, n_press, _, y| {
            if n_press == 2
                && let Some(row) = results.row_at_y(y as i32)
                && let Some(path) = ui.path_at(row.index())
            {
                ui.open_path(&path);
            }
        }
    });
    results.add_controller(double_click);

    // Ctrl+F focuses search, Esc clears it (then goes back to the list),
    // Ctrl+Enter opens the selected file, Shift+F10 / Menu shows its menu.
    let keys = gtk::EventControllerKey::new();
    keys.connect_key_pressed({
        let ui = ui.clone();
        move |_, key, _, state| {
            if state.contains(gtk::gdk::ModifierType::CONTROL_MASK)
                && (key == gtk::gdk::Key::f || key == gtk::gdk::Key::F)
            {
                ui.search.grab_focus();
                return glib::Propagation::Stop;
            }
            if state.contains(gtk::gdk::ModifierType::CONTROL_MASK)
                && (key == gtk::gdk::Key::Return || key == gtk::gdk::Key::KP_Enter)
            {
                ui.open_selected();
                return glib::Propagation::Stop;
            }
            if (key == gtk::gdk::Key::F10 && state.contains(gtk::gdk::ModifierType::SHIFT_MASK))
                || key == gtk::gdk::Key::Menu
            {
                ui.popup_selected();
                return glib::Propagation::Stop;
            }
            if key == gtk::gdk::Key::Escape {
                if !ui.search.text().is_empty() {
                    ui.search.set_text("");
                } else {
                    ui.split.set_show_sidebar(true);
                }
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        }
    });
    window.add_controller(keys);

    // Sidebar footer buttons: heavy work goes to a background thread.
    build_btn.connect_clicked({
        let ui = ui.clone();
        let index = index.clone();
        let refresh = refresh.clone();
        move |_| ui.start_reindex(&index, &refresh)
    });
    add_btn.connect_clicked({
        let ui = ui.clone();
        let index = index.clone();
        let refresh = refresh.clone();
        let window = window.clone();
        move |_| {
            let chooser = gtk::FileDialog::new();
            chooser.set_title("Add folder to optionSearch index");
            let ui = ui.clone();
            let index = index.clone();
            let refresh = refresh.clone();
            chooser.select_folder(Some(&window), None::<&gio::Cancellable>, move |result| {
                if let Ok(folder) = result {
                    match folder.path() {
                        Some(path) => ui.start_add_folder(&index, &refresh, path),
                        None => ui.toast("Selected folder has no local path"),
                    }
                }
            });
        }
    });
    clear_btn.connect_clicked({
        let ui = ui.clone();
        move |_| ui.clear_search()
    });

    // Overflow menu: shortcuts + about only.
    let act_shortcuts = gio::SimpleAction::new("shortcuts", None);
    act_shortcuts.connect_activate({
        let ui = ui.clone();
        move |_, _| {
            ui.toast(
                "Ctrl+F search · Enter preview · Ctrl+Enter open · Right-click more · Esc clear",
            );
        }
    });
    window.add_action(&act_shortcuts);

    let act_about = gio::SimpleAction::new("about", None);
    act_about.connect_activate({
        let window = window.clone();
        move |_, _| {
            let dialog = gtk::AboutDialog::builder()
                .program_name("optionSearch")
                .comments("Instant local file search. Everything stays on this machine.")
                .website("https://option.family")
                .transient_for(&window)
                .modal(true)
                .build();
            dialog.present();
        }
    });
    window.add_action(&act_about);

    // Audio controls: play toggles the stream, the scale seeks (user input
    // only — `change-value` doesn't fire on programmatic set_value), and a
    // tick keeps icon/scale/time in sync with the stream.
    play_btn.connect_clicked({
        let ui = ui.clone();
        move |_| {
            if let Some(media) = ui.media.borrow().as_ref() {
                if media.is_playing() {
                    media.pause();
                } else {
                    media.play();
                }
            }
        }
    });

    seek.connect_change_value({
        let ui = ui.clone();
        move |_, _, value| {
            if let Some(media) = ui.media.borrow().as_ref() {
                media.seek(value as i64);
            }
            glib::Propagation::Proceed
        }
    });

    let seek_press = gtk::GestureClick::new();
    seek_press.connect_pressed({
        let ui = ui.clone();
        move |_, _, _, _| ui.seeking.set(true)
    });
    seek_press.connect_released({
        let ui = ui.clone();
        move |_, _, _, _| ui.seeking.set(false)
    });
    seek_press.connect_stopped({
        let ui = ui.clone();
        move |_| ui.seeking.set(false)
    });
    seek.add_controller(seek_press);

    glib::timeout_add_local(Duration::from_millis(250), {
        let ui = ui.clone();
        move || {
            let media = ui.media.borrow().clone();
            let Some(media) = media else {
                return glib::ControlFlow::Continue;
            };
            if let Some(error) = media.error() {
                ui.toast(&format!("Cannot play audio: {error}"));
                ui.stop_media();
                return glib::ControlFlow::Continue;
            }
            let duration = media.duration();
            if duration > 0 {
                ui.seek.set_range(0.0, duration as f64);
                ui.seek.set_sensitive(true);
                if !ui.seeking.get() {
                    ui.seek.set_value(media.timestamp() as f64);
                }
                ui.time.set_text(&format!(
                    "{} / {}",
                    clock(media.timestamp()),
                    clock(duration)
                ));
            }
            ui.play_btn.set_icon_name(if media.is_playing() {
                "media-playback-pause-symbolic"
            } else {
                "media-playback-start-symbolic"
            });
            glib::ControlFlow::Continue
        }
    });

    window.present();

    // Keep the index current while the window is open: re-render the current
    // query whenever the engine generation moves.
    spawn_watcher(index.clone(), ui.clone(), refresh.clone());
}

struct Ui {
    results: gtk::ListBox,
    paths: RefCell<Vec<PathBuf>>,
    sidebar_stack: gtk::Stack,
    empty_page: adw::StatusPage,
    count: gtk::Label,
    detail_stack: gtk::Stack,
    icon: gtk::Image,
    title: gtk::Label,
    meta: gtk::Label,
    text: gtk::TextView,
    stack: gtk::Stack,
    picture: gtk::Picture,
    /// Current audio stream; `None` unless the preview page is "audio".
    media: RefCell<Option<gtk::MediaFile>>,
    play_btn: gtk::Button,
    seek: gtk::Scale,
    time: gtk::Label,
    /// True while the user is dragging the seek scale.
    seeking: Cell<bool>,
    search: gtk::SearchEntry,
    split: adw::OverlaySplitView,
    toasts: adw::ToastOverlay,
    spinner: gtk::Spinner,
    status: gtk::Label,
    progress: gtk::ProgressBar,
    progress_label: gtk::Label,
    build_btn: gtk::Button,
    add_btn: gtk::Button,
    busy: Cell<bool>,
}

impl Ui {
    fn toast(&self, message: &str) {
        self.toasts.add_toast(adw::Toast::new(message));
    }

    /// Starts an audio stream for the preview page. Playback is manual —
    /// the controls stay dimmed until the stream reports a duration.
    fn start_media(&self, path: &Path) {
        let media = gtk::MediaFile::for_filename(path);
        media.set_loop(false);
        *self.media.borrow_mut() = Some(media);
        self.play_btn.set_sensitive(true);
        self.seek.set_range(0.0, 1.0);
        self.seek.set_value(0.0);
        self.time.set_text("0:00 / 0:00");
    }

    /// Pauses and drops the current stream, resetting the audio controls.
    /// Called before every new preview so sound never outlives its card.
    fn stop_media(&self) {
        if let Some(media) = self.media.take() {
            media.pause();
        }
        self.play_btn.set_icon_name("media-playback-start-symbolic");
        self.play_btn.set_sensitive(false);
        self.seek.set_sensitive(false);
        self.seek.set_value(0.0);
        self.time.set_text("0:00 / 0:00");
    }

    fn render(self: &Rc<Self>, found: &[optionsearch_core::Hit], query: &str) {
        while let Some(child) = self.results.first_child() {
            self.results.remove(&child);
        }
        self.paths
            .replace(found.iter().map(|hit| hit.path.clone()).collect());
        if found.is_empty() {
            self.sidebar_stack.set_visible_child_name("empty");
            if query.is_empty() {
                self.empty_page.set_title("Start searching");
                self.empty_page.set_description(Some(
                    "Type above to search every indexed file. Everything stays on this machine.",
                ));
            } else {
                self.empty_page.set_title("No matches");
                self.empty_page
                    .set_description(Some("Try a different term or build the index again."));
            }
            return;
        }
        self.sidebar_stack.set_visible_child_name("results");
        self.count.set_text(&format!(
            "{} result{}",
            found.len(),
            if found.len() == 1 { "" } else { "s" }
        ));
        for item in found {
            let row = result_row(item);
            // Per-line "…" opener for the file menu (right-click works too).
            let more = gtk::Button::builder()
                .icon_name("view-more-symbolic")
                .tooltip_text("More actions for this file")
                .build();
            more.add_css_class("flat");
            more.add_css_class("circular");
            let anchor = more.clone();
            let path = item.path.clone();
            let this = self.clone();
            more.connect_clicked(move |_| this.popup_for_path(&path, &anchor));
            row.add_suffix(&more);
            self.results.append(&row);
        }
        // Keep the first row selected so Enter / keyboard flow feels instant.
        if let Some(first) = self.results.row_at_index(0) {
            self.results.select_row(Some(&first));
        }
    }

    fn path_at(&self, index: i32) -> Option<PathBuf> {
        self.paths.borrow().get(index as usize).cloned()
    }

    fn preview_first(&self) {
        if !self.paths.borrow().is_empty() {
            self.preview_row(0);
        }
    }

    fn preview_row(&self, index: i32) {
        let Some(path) = self.path_at(index) else {
            return;
        };
        show_preview(&path, self);
        // Focus the detail pane so Preview feels like a destination.
        self.text.grab_focus();
        // On narrow windows the detail covers the list; jumping there
        // keeps the preview visible.
        if self.split.is_collapsed() {
            self.split.set_show_sidebar(false);
        }
    }

    /// Read-only file actions: open, reveal, copy. No
    /// delete/move/rename by design.
    fn open_path(&self, path: &Path) {
        let uri = gtk::gio::File::for_path(path).uri().to_string();
        if let Err(error) =
            gtk::gio::AppInfo::launch_default_for_uri(&uri, None::<&gtk::gio::AppLaunchContext>)
        {
            self.toast(&format!("Could not open {}: {error}", path.display()));
        }
    }

    fn reveal_path(&self, path: &Path) {
        // Open the parent in the file manager. Selecting the file itself
        // would need a FileManager1 ShowItems D-Bus call, so the folder
        // open is the portable fallback.
        let parent = path.parent().unwrap_or(path);
        let uri = gtk::gio::File::for_path(parent).uri().to_string();
        if let Err(error) =
            gtk::gio::AppInfo::launch_default_for_uri(&uri, None::<&gtk::gio::AppLaunchContext>)
        {
            self.toast(&format!("Could not open folder: {error}"));
        }
    }

    fn copy_path_text(&self, path: &Path) {
        if let Some(display) = gtk::gdk::Display::default() {
            display.clipboard().set_text(&path.display().to_string());
            self.toast("Path copied");
        }
    }

    fn copy_file(&self, path: &Path) {
        // Advertise the file as a uri-list so Files/paste targets accept it.
        let uri = gtk::gio::File::for_path(path).uri();
        let bytes = glib::Bytes::from(format!("{uri}\r\n").as_bytes());
        let provider = gtk::gdk::ContentProvider::for_bytes("text/uri-list", &bytes);
        if let Some(display) = gtk::gdk::Display::default() {
            let _ = display.clipboard().set_content(Some(&provider));
            self.toast("File copied — paste it in Files");
        }
    }

    fn open_selected(&self) {
        let index = self
            .results
            .selected_row()
            .map(|row| row.index())
            .unwrap_or(0);
        if let Some(path) = self.path_at(index) {
            self.open_path(&path);
        }
    }

    fn popup_selected(self: &Rc<Self>) {
        if let Some(row) = self.results.selected_row() {
            if let Some(path) = self.path_at(row.index()) {
                self.popup_for_path(&path, &row);
                return;
            }
        }
        if let Some(path) = self.path_at(0) {
            self.popup_for_path(&path, &self.results);
        }
    }

    /// Per-file context menu: Open, Open containing folder, Copy path,
    /// Copy file, Preview. Built fresh per invocation so actions capture
    /// exactly one path; destroyed when closed.
    fn popup_for_path(self: &Rc<Self>, path: &Path, anchor: &impl IsA<gtk::Widget>) {
        let menu = gio::Menu::new();
        menu.append(Some("Open"), Some("ctx.open"));
        menu.append(Some("Open containing folder"), Some("ctx.reveal"));
        menu.append(Some("Copy path"), Some("ctx.copy-path"));
        menu.append(Some("Copy file"), Some("ctx.copy-file"));
        menu.append(Some("Preview"), Some("ctx.preview"));

        let group = gio::SimpleActionGroup::new();
        let add = |name: &str, run: Box<dyn Fn()>| {
            let action = gio::SimpleAction::new(name, None);
            action.connect_activate(move |_, _| run());
            group.add_action(&action);
        };
        {
            let this = self.clone();
            let path = path.to_path_buf();
            add("open", Box::new(move || this.open_path(&path)));
        }
        {
            let this = self.clone();
            let path = path.to_path_buf();
            add("reveal", Box::new(move || this.reveal_path(&path)));
        }
        {
            let this = self.clone();
            let path = path.to_path_buf();
            add("copy-path", Box::new(move || this.copy_path_text(&path)));
        }
        {
            let this = self.clone();
            let path = path.to_path_buf();
            add("copy-file", Box::new(move || this.copy_file(&path)));
        }
        {
            let this = self.clone();
            let known: Vec<PathBuf> = self.paths.borrow().clone();
            let path = path.to_path_buf();
            add(
                "preview",
                Box::new(move || {
                    let index = known.iter().position(|p| p == &path).unwrap_or(0) as i32;
                    this.preview_row(index);
                }),
            );
        }

        let popover = gtk::PopoverMenu::from_model(Some(&menu));
        popover.insert_action_group("ctx", Some(&group));
        popover.set_parent(anchor);
        popover.popup();
        popover.connect_closed(|popover| popover.unparent());
    }

    fn clear_search(&self) {
        self.search.set_text("");
        self.stop_media();
        self.detail_stack.set_visible_child_name("empty");
        self.toast("Search cleared");
    }

    /// Flip the sidebar footer between idle and working states.
    fn set_busy(&self, busy: bool, phase: &str) {
        self.busy.set(busy);
        self.spinner.set_visible(busy);
        if busy {
            self.spinner.start();
        } else {
            self.spinner.stop();
        }
        self.progress.set_visible(busy);
        self.progress_label.set_visible(busy);
        if busy {
            self.progress_label.set_text(phase);
            self.status.set_text(phase);
        }
        self.build_btn.set_sensitive(!busy);
        self.build_btn.set_tooltip_text(Some(if busy {
            "Already indexing — wait for it to finish"
        } else {
            "Rebuild the whole index in the background"
        }));
        self.add_btn.set_sensitive(!busy);
    }

    fn pulse_while_busy(self: &Rc<Self>) {
        let this = self.clone();
        glib::timeout_add_local(Duration::from_millis(120), move || {
            if this.busy.get() {
                this.progress.pulse();
                glib::ControlFlow::Continue
            } else {
                glib::ControlFlow::Break
            }
        });
    }

    /// Full reindex on a background thread: the scan (slow part) runs
    /// off-thread while search keeps hitting the current index under a
    /// short read lock. Results travel back through a `MainContext` channel
    /// so GTK widgets are only touched on the main loop.
    /// Permission errors are counted inside the scan and skipped, never a popup.
    fn start_reindex(self: &Rc<Self>, index: &Arc<Engine>, refresh: &Refresh) {
        if self.busy.get() {
            return;
        }
        self.set_busy(true, "Indexing…");
        self.pulse_while_busy();

        let query = self.search.text().to_string();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<Result<(usize, usize), String>>();
        let this = self.clone();
        let refresh = refresh.clone();
        let index_main = Arc::clone(index);
        // Poll the mailbox from the main loop: the closure stays on the GTK
        // thread (so it may hold `Rc`), the worker only sends plain data.
        glib::timeout_add_local(Duration::from_millis(100), move || {
            match done_rx.try_recv() {
                Ok(Ok((files, _dirs))) => {
                    refresh(&query);
                    this.status.set_text(&stats_text(&index_main));
                    this.toast(&format!("Index ready — {files} files"));
                    this.set_busy(false, "");
                    glib::ControlFlow::Break
                }
                Ok(Err(error)) => {
                    this.toast(&format!("Index failed: {error}"));
                    this.set_busy(false, "");
                    glib::ControlFlow::Break
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    this.set_busy(false, "");
                    glib::ControlFlow::Break
                }
            }
        });

        let index_bg = Arc::clone(index);
        std::thread::spawn(move || {
            let outcome = index_bg
                .reindex()
                .map(|stats| (stats.files, stats.dirs))
                .map_err(|error| format!("{error:#}"));
            let _ = done_tx.send(outcome);
        });
    }

    /// Same background pattern for adding a folder (its scan also blocks).
    fn start_add_folder(self: &Rc<Self>, index: &Arc<Engine>, refresh: &Refresh, path: PathBuf) {
        if self.busy.get() {
            self.toast("Already indexing — wait for it to finish");
            return;
        }
        self.set_busy(true, "Adding folder…");
        self.pulse_while_busy();

        let query = self.search.text().to_string();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let this = self.clone();
        let refresh = refresh.clone();
        let index_main = Arc::clone(index);
        glib::timeout_add_local(Duration::from_millis(100), move || {
            match done_rx.try_recv() {
                Ok(Ok(())) => {
                    refresh(&query);
                    this.status.set_text(&stats_text(&index_main));
                    this.toast("Folder added to the index");
                    this.set_busy(false, "");
                    glib::ControlFlow::Break
                }
                Ok(Err(error)) => {
                    this.toast(&format!("Could not add folder: {error}"));
                    this.set_busy(false, "");
                    glib::ControlFlow::Break
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    this.set_busy(false, "");
                    glib::ControlFlow::Break
                }
            }
        });

        let index_bg = Arc::clone(index);
        std::thread::spawn(move || {
            let outcome = index_bg
                .add_root(path)
                .map_err(|error| format!("{error:#}"));
            let _ = done_tx.send(outcome);
        });
    }
}

fn stats_text(index: &Engine) -> String {
    let stats = index.stats();
    if stats.live_entries == 0 {
        "Index is empty — build it to get started".to_owned()
    } else {
        format!("{} files · {} folders", stats.live_entries, stats.dirs)
    }
}

fn result_row(item: &optionsearch_core::Hit) -> adw::ActionRow {
    let name = item
        .path
        .file_name()
        .and_then(|x| x.to_str())
        .unwrap_or("/")
        .to_owned();
    let detail = format!(
        "{}  ·  {}  ·  {}",
        if item.is_dir { "directory" } else { "file" },
        human_size(item.size),
        item.path.display()
    );
    let row = adw::ActionRow::builder()
        .title(name)
        .subtitle(detail)
        .activatable(true)
        .build();
    let icon = gtk::Image::from_gicon(&gio::ThemedIcon::new(icon_name_for(
        &item.path,
        item.is_dir,
    )));
    icon.set_pixel_size(32);
    row.add_prefix(&icon);
    row
}

fn show_preview(path: &Path, ui: &Ui) {
    let preview = optionsearch_core::preview::preview(path, &PreviewLimits::default());
    let name = path
        .file_name()
        .and_then(|x| x.to_str())
        .unwrap_or("/")
        .to_owned();
    ui.title.set_text(&name);
    ui.icon
        .set_from_gicon(&gio::ThemedIcon::new(icon_name_for(path, false)));
    ui.detail_stack.set_visible_child_name("preview");
    ui.stop_media();
    match preview {
        Preview::Image {
            path: image,
            width,
            height,
        } => {
            // SVGs may carry no declared size; skip "0×0" in that case.
            if width > 0 && height > 0 {
                ui.meta
                    .set_text(&format!("{} · {width}×{height}", image.display()));
            } else {
                ui.meta.set_text(&image.display().to_string());
            }
            ui.picture.set_file(Some(&gtk::gio::File::for_path(&image)));
            ui.stack.set_visible_child_name("image");
        }
        Preview::Audio { .. } => {
            ui.meta.set_text(&format!(
                "{} · {}",
                mime_guess::from_path(path)
                    .first_or_octet_stream()
                    .essence_str(),
                path.display()
            ));
            ui.picture.set_file(Option::<&gtk::gio::File>::None);
            ui.start_media(path);
            ui.stack.set_visible_child_name("audio");
        }
        Preview::Text {
            content, truncated, ..
        } => {
            ui.meta.set_text(&path.display().to_string());
            ui.picture.set_file(Option::<&gtk::gio::File>::None);
            ui.stack.set_visible_child_name("text");
            let mut body = content;
            if truncated {
                body.push_str("\n… preview truncated");
            }
            ui.text.buffer().set_text(&body);
        }
        Preview::Pdf {
            text: pdf_text,
            pages,
            ..
        } => {
            ui.meta.set_text(&path.display().to_string());
            ui.picture.set_file(Option::<&gtk::gio::File>::None);
            ui.stack.set_visible_child_name("text");
            ui.text
                .buffer()
                .set_text(&format!("PDF · {pages} pages\n\n{pdf_text}"));
        }
        other => {
            ui.meta.set_text(&path.display().to_string());
            ui.picture.set_file(Option::<&gtk::gio::File>::None);
            ui.stack.set_visible_child_name("text");
            let body = match other {
                Preview::Meta(m) => format!(
                    "{}\n\n{} bytes · mode {:o} · owner {}\n{}",
                    m.kind,
                    m.size,
                    m.mode & 0o7777,
                    m.owner,
                    m.path.display()
                ),
                Preview::Error(e) => {
                    ui.toast(&format!("Preview unavailable: {e}"));
                    format!("Preview unavailable\n{e}")
                }
                _ => String::new(),
            };
            ui.text.buffer().set_text(&body);
        }
    }
}

/// File-type icon from the system icon theme (symbolic names only, so the
/// row follows whatever theme the system uses).
fn icon_name_for(path: &Path, is_dir: bool) -> &'static str {
    if is_dir {
        return "folder-symbolic";
    }
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    match (mime.type_().as_str(), mime.subtype().as_str()) {
        ("image", _) => "image-x-generic-symbolic",
        ("audio", _) => "audio-x-generic-symbolic",
        ("video", _) => "video-x-generic-symbolic",
        ("application", "pdf") => "application-pdf-symbolic",
        ("text", _) => "text-x-generic-symbolic",
        _ => match path
            .extension()
            .and_then(|x| x.to_str())
            .unwrap_or("")
            .to_lowercase()
            .as_str()
        {
            "pdf" => "application-pdf-symbolic",
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "bmp" | "tiff" | "ico" => {
                "image-x-generic-symbolic"
            }
            "mp3" | "ogg" | "oga" | "opus" | "flac" | "wav" | "m4a" | "aac" | "aiff" | "aif" => {
                "audio-x-generic-symbolic"
            }
            "mp4" | "mkv" | "webm" | "avi" => "video-x-generic-symbolic",
            "zip" | "tar" | "gz" | "7z" | "rar" => "package-x-generic-symbolic",
            _ => "text-x-generic-symbolic",
        },
    }
}

/// Start the inotify watcher so the index (and thus results) stay current
/// while the window is open. The engine is shared; the watcher runs on its own
/// worker thread and applies events in batches.
fn spawn_watcher(index: Arc<Engine>, ui: Rc<Ui>, refresh: Refresh) {
    // Process-wide watcher: a no-op when daemon mode already owns it. Kept
    // alive in ENGINE_WATCHER — dropping a Watcher stops its thread.
    ensure_engine_watcher(&index);
    // A lightweight tick re-renders the current query whenever the engine
    // generation moves, so the list stays honest about what is watched.
    let seen = Rc::new(Cell::new(index.generation()));
    glib::timeout_add_local(Duration::from_secs(2), move || {
        if index.generation() != seen.get() {
            seen.set(index.generation());
            refresh(&ui.search.text());
            ui.status.set_text(&stats_text(&index));
        }
        glib::ControlFlow::Continue
    });
}

/// The translucency effect stays on the sidebar only: the window itself is
/// transparent so the tinted sidebar can show through, while the detail pane
/// and preview surfaces pin themselves to the opaque named colors.
fn install_css(display: &gtk::gdk::Display) {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(
        ".optionsearch-opaque { background-color: transparent; }
         .optionsearch-opaque .sidebar-column,
         .optionsearch-opaque .sidebar-column headerbar { background-color: transparent; }
         .optionsearch-opaque .detail-pane { background-color: @window_bg_color; }
         .optionsearch-opaque .detail-pane headerbar { background-color: @headerbar_bg_color; }
         .optionsearch-opaque .card { background-color: @card_bg_color; }
         .optionsearch-opaque textview text { background-color: @view_bg_color; }
         .optionsearch-opaque picture { background-color: @view_bg_color; }",
    );
    gtk::style_context_add_provider_for_display(
        display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

/// Formats a media timestamp (µs) as `m:ss`.
fn clock(micros: i64) -> String {
    let seconds = micros.max(0) / 1_000_000;
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

fn human_size(size: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let (mut n, mut i) = (size as f64, 0);
    while n >= 1024. && i < 4 {
        n /= 1024.;
        i += 1;
    }
    if i == 0 {
        format!("{size} {}", U[i])
    } else {
        format!("{n:.1} {}", U[i])
    }
}
