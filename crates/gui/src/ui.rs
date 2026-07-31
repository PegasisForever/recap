//! The window. Everything is here: add monitors, pick a microphone, set up
//! storage, record, copy the link. Nothing needs a config file or a terminal.

use adw::prelude::*;
use gtk::glib;
use recap_core::config::{Config, Source};
use recap_core::progress::Progress;
use recap_core::record::{self, AudioTarget, RecordOptions, Recording};
use recap_core::s3;
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

const APP_ID: &str = "site.pegasis.Recap";

/// What the worker thread sends back while finishing a recording.
enum Step {
    Tick(Progress),
    Done(String),
    Failed(String),
}

/// Monitors are numbered in the order they were granted. Names like "left" and
/// "right" go stale the moment a display moves, and the portal never tells us
/// where one physically sits.
fn next_label(cfg: &Config) -> String {
    let mut n = cfg.sources.len() + 1;
    while cfg.sources.iter().any(|s| s.label == format!("Monitor {n}")) {
        n += 1;
    }
    format!("Monitor {n}")
}

fn mic_target(cfg: &Config) -> AudioTarget {
    match cfg.mic_node {
        None => AudioTarget::DefaultSource,
        Some(0) => AudioTarget::None,
        Some(id) => AudioTarget::Node(id),
    }
}

struct State {
    cfg: Config,
    live: Option<Recording>,
    outdir: Option<PathBuf>,
    id: String,
    /// Ticks the elapsed time onto the Stop button once a second.
    timer: Option<glib::SourceId>,
}

fn elapsed(secs: u64) -> String {
    if secs >= 3600 {
        format!("{}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
    } else {
        format!("{}:{:02}", secs / 60, secs % 60)
    }
}

type Shared = Rc<RefCell<State>>;

pub fn run() -> anyhow::Result<()> {
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(build);
    app.run_with_args::<&str>(&[]);
    Ok(())
}

fn build(app: &adw::Application) {
    let state: Shared = Rc::new(RefCell::new(State {
        cfg: Config::load().unwrap_or_default(),
        live: None,
        outdir: None,
        id: String::new(),
        timer: None,
    }));

    let monitors = adw::PreferencesGroup::builder().title("Monitors").build();

    let add = gtk::Button::builder()
        .label("Add monitor")
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .build();
    monitors.set_header_suffix(Some(&add));

    // Transient notices go to a toast rather than a line of text that sits
    // there saying "Ready" when there is nothing to say.
    let toasts = adw::ToastOverlay::new();
    let say = {
        let toasts = toasts.clone();
        Rc::new(move |msg: &str| toasts.add_toast(adw::Toast::new(msg)))
    };

    let record_btn = gtk::Button::builder()
        .label("Record")
        .height_request(48)
        .css_classes(["suggested-action", "pill"])
        .build();

    let link_entry = gtk::Entry::builder()
        .editable(false)
        .hexpand(true)
        .placeholder_text("Link appears here")
        .build();
    let copy_btn = gtk::Button::builder()
        .icon_name("edit-copy-symbolic")
        .tooltip_text("Copy link")
        .build();
    let link_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .visible(false)
        .build();
    link_row.append(&link_entry);
    link_row.append(&copy_btn);

    // The microphone changes from one recording to the next, unlike a bucket
    // name, so it belongs on the main screen rather than behind Settings.
    let audio = adw::PreferencesGroup::builder().title("Audio").build();
    let mic = adw::ComboRow::builder().title("Microphone").build();
    audio.add(&mic);

    let mic_sources: Rc<RefCell<Vec<(u32, String)>>> = Rc::new(RefCell::new(Vec::new()));
    // Swapping the model fires selected-notify, which would otherwise write the
    // reset selection straight back into the config.
    let quiet = Rc::new(std::cell::Cell::new(false));

    let repopulate: Rc<dyn Fn()> = {
        let state = state.clone();
        let mic = mic.clone();
        let mic_sources = mic_sources.clone();
        let quiet = quiet.clone();
        Rc::new(move || {
            let fresh = record::list_audio_sources();
            if mic.model().is_some() && *mic_sources.borrow() == fresh {
                return;
            }
            let mut names = vec!["System default".to_string(), "No microphone".to_string()];
            names.extend(fresh.iter().map(|(_, n)| n.clone()));
            let model =
                gtk::StringList::new(&names.iter().map(String::as_str).collect::<Vec<_>>());
            let selected = match state.borrow().cfg.mic_node {
                None => 0,
                Some(0) => 1,
                Some(id) => fresh
                    .iter()
                    .position(|(i, _)| *i == id)
                    .map(|p| p as u32 + 2)
                    .unwrap_or(0),
            };
            *mic_sources.borrow_mut() = fresh;
            quiet.set(true);
            mic.set_model(Some(&model));
            mic.set_selected(selected);
            quiet.set(false);
        })
    };
    repopulate();

    mic.connect_selected_notify({
        let state = state.clone();
        let mic_sources = mic_sources.clone();
        let quiet = quiet.clone();
        move |row| {
            if quiet.get() {
                return;
            }
            let mut s = state.borrow_mut();
            s.cfg.mic_node = match row.selected() {
                0 => None,
                1 => Some(0),
                n => mic_sources.borrow().get(n as usize - 2).map(|(id, _)| *id),
            };
            let _ = s.cfg.save();
        }
    });

    // Anything that would stop a recording sits at the top, permanently, until
    // it is fixed. A toast would scroll away before the user could act on it.
    let problems = adw::PreferencesGroup::builder()
        .title("Fix before recording")
        .visible(false)
        .build();
    let problem_rows: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::new(RefCell::new(Vec::new()));

    let page = adw::PreferencesPage::new();
    page.add(&problems);
    page.add(&monitors);
    page.add(&audio);

    let actions = adw::PreferencesGroup::new();
    let vbox = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .build();
    // Stopping an hours-long recording compresses two audio tracks, rewrites
    // each video's index and then pushes gigabytes. Every one of those is
    // minutes of nothing to look at without this.
    let progress = gtk::ProgressBar::builder()
        .show_text(true)
        .text("")
        .visible(false)
        .build();

    // GtkProgressBar has no transition of its own, so setting a fraction is an
    // instant jump. Stages land in lumps, which makes the bar look like it is
    // stuttering rather than working. Instead each update sets a target and the
    // frame clock eases the shown value towards it.
    let bar_target = Rc::new(std::cell::Cell::new(0.0f64));
    let bar_phase = Rc::new(RefCell::new(String::new()));
    {
        let target = bar_target.clone();
        let phase = bar_phase.clone();
        let last_frame = std::cell::Cell::new(0i64);
        progress.add_tick_callback(move |bar, clock| {
            let now = clock.frame_time();
            let prev = last_frame.replace(now);
            // Exponential ease with a time constant, so the speed does not
            // depend on the refresh rate and a long jump still lands quickly.
            let dt = if prev == 0 { 0.0 } else { (now - prev).max(0) as f64 / 1e6 };
            const TAU: f64 = 0.12;

            let want = target.get();
            let have = bar.fraction();
            let gap = want - have;
            let shown = if gap.abs() < 0.001 || dt <= 0.0 {
                want
            } else {
                have + gap * (1.0 - (-dt / TAU).exp())
            };
            if (shown - have).abs() > f64::EPSILON {
                bar.set_fraction(shown);
            }
            // The label follows the animated value, not the target, so the
            // number and the bar never disagree.
            bar.set_text(Some(&format!("{}  {:.0}%", phase.borrow(), shown * 100.0)));
            glib::ControlFlow::Continue
        });
    }

    vbox.append(&record_btn);
    vbox.append(&progress);
    vbox.append(&link_row);
    actions.add(&vbox);
    page.add(&actions);
    toasts.set_child(Some(&page));

    let settings_btn = gtk::Button::builder()
        .icon_name("emblem-system-symbolic")
        .tooltip_text("Settings")
        .build();
    let header = adw::HeaderBar::new();
    header.pack_end(&settings_btn);
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&toasts));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Recap")
        .default_width(460)
        .default_height(560)
        .content(&toolbar)
        .build();

    let recheck: Rc<dyn Fn()> = {
        let state = state.clone();
        let problems = problems.clone();
        let problem_rows = problem_rows.clone();
        let record_btn = record_btn.clone();
        Rc::new(move || {
            for r in problem_rows.borrow_mut().drain(..) {
                problems.remove(&r);
            }
            let issues = recap_core::check::run(&state.borrow().cfg);
            for issue in &issues {
                let row = adw::ActionRow::builder()
                    .title(&issue.title)
                    .subtitle(&issue.detail)
                    .build();
                row.add_prefix(&gtk::Image::from_icon_name("dialog-warning-symbolic"));
                problems.add(&row);
                problem_rows.borrow_mut().push(row);
            }
            problems.set_visible(!issues.is_empty());
            // Never fight the upload path for control of the button.
            if state.borrow().live.is_none() && record_btn.label().as_deref() == Some("Record") {
                record_btn.set_sensitive(issues.is_empty());
            }
        })
    };
    recheck();

    // A microphone plugged in while the window sat in the background should
    // appear when the user comes back to it. Settings closing lands here too,
    // which is when a newly typed bucket name should clear its warning.
    window.connect_is_active_notify({
        let repopulate = repopulate.clone();
        let recheck = recheck.clone();
        move |w| {
            if w.is_active() {
                repopulate();
                recheck();
            }
        }
    });

    let rows: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::new(RefCell::new(Vec::new()));
    // Rows rebuild the very list they live in, so the rebuild closure is
    // reached through a cell rather than captured directly.
    let refresher: Rc<RefCell<Option<Rc<dyn Fn()>>>> = Rc::new(RefCell::new(None));
    let refresh: Rc<dyn Fn()> = {
        let state = state.clone();
        let monitors = monitors.clone();
        let rows = rows.clone();
        let refresher = refresher.clone();
        let recheck = recheck.clone();
        Rc::new(move || {
            for r in rows.borrow_mut().drain(..) {
                monitors.remove(&r);
            }
            let sources = state.borrow().cfg.sources.clone();
            for (i, src) in sources.iter().enumerate() {
                let row = adw::ActionRow::builder()
                    .title(&src.label)
                    .subtitle(&src.resolution())
                    .build();
                let remove = gtk::Button::builder()
                    .icon_name("user-trash-symbolic")
                    .tooltip_text(format!("Remove {}", src.label))
                    .valign(gtk::Align::Center)
                    .css_classes(["flat"])
                    .build();
                remove.connect_clicked({
                    let state = state.clone();
                    let refresher = refresher.clone();
                    let recheck = recheck.clone();
                    move |_| {
                        {
                            let mut s = state.borrow_mut();
                            if i < s.cfg.sources.len() {
                                let gone = s.cfg.sources.remove(i);
                                // The grant itself lives in GNOME's permission
                                // store. Dropping our token only makes us
                                // forget which grant to ask for.
                                let _ = std::fs::remove_file(Config::token_path(&gone.id));
                            }
                            let _ = s.cfg.save();
                        }
                        let f = refresher.borrow().clone();
                        if let Some(f) = f {
                            f();
                        }
                        recheck();
                    }
                });
                row.add_suffix(&remove);
                monitors.add(&row);
                rows.borrow_mut().push(row);
            }
        })
    };
    *refresher.borrow_mut() = Some(refresh.clone());
    refresh();

    settings_btn.connect_clicked({
        let state = state.clone();
        let window = window.clone();
        move |_| preferences(&window, state.clone())
    });

    // Granting waits on a compositor dialog, so it cannot run on the main
    // thread or the window would freeze with the dialog stuck behind it.
    add.connect_clicked({
        let state = state.clone();
        let say = say.clone();
        let refresh = refresh.clone();
        let recheck = recheck.clone();
        let add = add.clone();
        move |_| {
            let label = next_label(&state.borrow().cfg);
            add.set_sensitive(false);

            let (tx, rx) = async_channel::bounded(1);
            let id = uuid::Uuid::new_v4().to_string();
            {
                let id = id.clone();
                std::thread::spawn(move || {
                    let _ =
                        tx.send_blocking(record::add_source(&id, 180).map_err(|e| e.to_string()));
                });
            }
            glib::spawn_future_local({
                let state = state.clone();
                let say = say.clone();
                let refresh = refresh.clone();
                let recheck = recheck.clone();
                let add = add.clone();
                async move {
                    match rx.recv().await {
                        Ok(Ok((w, h))) => {
                            {
                                let mut s = state.borrow_mut();
                                s.cfg.sources.push(Source {
                                    id,
                                    label: label.clone(),
                                    width: w,
                                    height: h,
                                });
                                let _ = s.cfg.save();
                            }
                            refresh();
                            recheck();
                            say(&format!("{label} added"));
                        }
                        Ok(Err(e)) => say(&format!("Not added: {e}")),
                        Err(_) => say("Not added"),
                    }
                    add.set_sensitive(true);
                }
            });
        }
    });

    record_btn.connect_clicked({
        let state = state.clone();
        let say = say.clone();
        let link_row = link_row.clone();
        let link_entry = link_entry.clone();
        let rows = rows.clone();
        move |btn| {
            let recording = state.borrow().live.is_some();
            if !recording {
                // The startup check already disables this button when anything
                // is missing, and says why at the top of the window.
                let id = uuid::Uuid::new_v4().to_string();
                let outdir = recap_core::config::Config::staging_dir().join(&id);
                let started = {
                    let s = state.borrow();
                    let opts = RecordOptions {
                        outdir: outdir.clone(),
                        fps: 30,
                        mic: mic_target(&s.cfg),
                        system: AudioTarget::DefaultSink,
                        allow_cpu_encoding: true,
                    };
                    record::start(&s.cfg, &opts)
                };
                match started {
                    Ok(rec) => {
                        link_row.set_visible(false);
                        btn.set_label("Stop · 0:00");
                        btn.remove_css_class("suggested-action");
                        btn.add_css_class("destructive-action");
                        // The button carries the elapsed time, so there is no
                        // separate status line to keep in sync.
                        let since = std::time::Instant::now();
                        let tick = glib::timeout_add_seconds_local(1, {
                            let btn = btn.clone();
                            let rows = rows.clone();
                            let state = state.clone();
                            move || {
                                btn.set_label(&format!(
                                    "Stop · {}",
                                    elapsed(since.elapsed().as_secs())
                                ));
                                // Which encoder each monitor got. Unknown for the
                                // first second or two, while the portal session is
                                // restored and before any frame is encoded.
                                if let Ok(s) = state.try_borrow() {
                                    if let Some(rec) = s.live.as_ref() {
                                        let encs = rec.encodings();
                                        for (i, row) in rows.borrow().iter().enumerate() {
                                            let Some(src) = s.cfg.sources.get(i) else {
                                                continue;
                                            };
                                            row.set_subtitle(&match encs.get(i).copied().flatten() {
                                                Some(e) => {
                                                    format!("{} · {}", src.resolution(), e.label())
                                                }
                                                None => src.resolution(),
                                            });
                                        }
                                    }
                                }
                                glib::ControlFlow::Continue
                            }
                        });
                        let mut s = state.borrow_mut();
                        s.live = Some(rec);
                        s.outdir = Some(outdir);
                        s.id = id;
                        s.timer = Some(tick);
                    }
                    Err(e) => say(&format!("{e}")),
                }
                return;
            }

            let (rec, id, outdir, cfg) = {
                let mut s = state.borrow_mut();
                if let Some(t) = s.timer.take() {
                    t.remove();
                }
                (
                    s.live.take().unwrap(),
                    s.id.clone(),
                    s.outdir.take(),
                    s.cfg.clone(),
                )
            };
            btn.remove_css_class("destructive-action");
            btn.add_css_class("suggested-action");
            btn.set_sensitive(false);
            btn.set_label("Record");
            // The encoder label describes a recording that is now over, so drop
            // it rather than leave it looking like a live reading.
            for (i, row) in rows.borrow().iter().enumerate() {
                if let Some(src) = cfg.sources.get(i) {
                    row.set_subtitle(&src.resolution());
                }
            }
            progress.set_visible(true);
            progress.set_fraction(0.0);
            bar_target.set(0.0);
            *bar_phase.borrow_mut() = "Closing the capture files".into();

            let started = rec.started;
            // Stopping waits on every capture process, compresses the audio and
            // rewrites each video index. On a long recording that is minutes of
            // work, so it cannot run on the main thread.
            let (tx, rx) = async_channel::bounded(64);
            std::thread::spawn(move || {
                let tx_stop = tx.clone();
                let mut report = move |p: Progress| {
                    // Dropping updates when the channel is full is correct: the
                    // next one supersedes them anyway.
                    let _ = tx_stop.try_send(Step::Tick(p));
                };
                let total_stages = rec.finish_stages();
                let stop_stages = rec.stop_stages();
                let parts = match rec.stop(1, total_stages, &mut report) {
                    Ok(p) => p,
                    Err(e) => {
                        let _ = tx.send_blocking(Step::Failed(e.to_string()));
                        return;
                    }
                };
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("tokio runtime");
                let tx_up = tx.clone();
                let mut report = move |p: Progress| {
                    let _ = tx_up.try_send(Step::Tick(p));
                };
                let out = rt.block_on(s3::upload_recording(
                    &cfg, &id, started, parts, stop_stages + 1, total_stages, &mut report));
                if let Some(dir) = outdir {
                    let _ = std::fs::remove_dir_all(dir);
                }
                let _ = tx.send_blocking(match out {
                    Ok((_, link)) => Step::Done(link),
                    Err(e) => Step::Failed(e.to_string()),
                });
            });

            glib::spawn_future_local({
                let say = say.clone();
                let link_row = link_row.clone();
                let link_entry = link_entry.clone();
                let btn = btn.clone();
                let progress = progress.clone();
                let bar_target = bar_target.clone();
                let bar_phase = bar_phase.clone();
                async move {
                    while let Ok(step) = rx.recv().await {
                        match step {
                            Step::Tick(p) => {
                                // The bar shows the whole finish, not the
                                // current stage. Per-stage fractions fill to
                                // 100% and then sit there until the next stage
                                // reports, which reads as a stall.
                                *bar_phase.borrow_mut() = p.phase.clone();
                                if let Some(f) = p.overall() {
                                    // Only ever move forwards. An out-of-order
                                    // update from the background remux would
                                    // otherwise drag the bar back.
                                    if f > bar_target.get() {
                                        bar_target.set(f);
                                    }
                                }
                            }
                            Step::Done(link) => {
                                link_entry.set_text(&link);
                                link_row.set_visible(true);
                                if let Some(d) = gtk::gdk::Display::default() {
                                    d.clipboard().set_text(&link);
                                }
                                say("Link copied to the clipboard");
                                progress.set_visible(false);
                                btn.set_sensitive(true);
                                break;
                            }
                            Step::Failed(e) => {
                                say(&e);
                                progress.set_visible(false);
                                btn.set_sensitive(true);
                                break;
                            }
                        }
                    }
                }
            });
        }
    });

    copy_btn.connect_clicked({
        let link_entry = link_entry.clone();
        let say = say.clone();
        move |_| {
            if let Some(d) = gtk::gdk::Display::default() {
                d.clipboard().set_text(&link_entry.text());
                say("Link copied");
            }
        }
    });

    window.present();
}

/// Everything that used to need a text editor.
fn preferences(parent: &adw::ApplicationWindow, state: Shared) {
    let win = adw::PreferencesWindow::builder()
        .transient_for(parent)
        .modal(true)
        .title("Settings")
        .search_enabled(false)
        .build();

    let page = adw::PreferencesPage::new();

    // --- storage -----------------------------------------------------------
    let s3_group = adw::PreferencesGroup::builder()
        .title("Storage")
        .description("Where recordings are uploaded. The link you copy is a presigned URL.")
        .build();

    macro_rules! text_row {
        ($title:expr, $get:expr, $set:expr) => {{
            let row = adw::EntryRow::builder().title($title).build();
            row.set_text(&$get(&state.borrow().cfg));
            row.connect_changed({
                let state = state.clone();
                move |e| {
                    let mut s = state.borrow_mut();
                    $set(&mut s.cfg, e.text().to_string());
                    let _ = s.cfg.save();
                }
            });
            s3_group.add(&row);
        }};
    }

    text_row!("Bucket", |c: &Config| c.s3.bucket.clone(), |c: &mut Config,
                                                           v| c
        .s3
        .bucket = v);
    text_row!(
        "Endpoint (blank for AWS)",
        |c: &Config| c.s3.endpoint.clone(),
        |c: &mut Config, v| c.s3.endpoint = v
    );
    text_row!("Region", |c: &Config| c.s3.region.clone(), |c: &mut Config,
                                                           v| c
        .s3
        .region = v);
    text_row!(
        "Access key",
        |c: &Config| c.s3.access_key.clone(),
        |c: &mut Config, v| c.s3.access_key = v
    );

    let secret = adw::PasswordEntryRow::builder().title("Secret key").build();
    secret.set_text(&state.borrow().cfg.s3.secret_key);
    secret.connect_changed({
        let state = state.clone();
        move |e| {
            let mut s = state.borrow_mut();
            s.cfg.s3.secret_key = e.text().to_string();
            let _ = s.cfg.save();
        }
    });
    s3_group.add(&secret);

    let path_style = adw::SwitchRow::builder()
        .title("Path-style addressing")
        .subtitle("Needed by MinIO and most self-hosted gateways")
        .active(state.borrow().cfg.s3.path_style)
        .build();
    path_style.connect_active_notify({
        let state = state.clone();
        move |r| {
            let mut s = state.borrow_mut();
            s.cfg.s3.path_style = r.is_active();
            let _ = s.cfg.save();
        }
    });
    s3_group.add(&path_style);
    page.add(&s3_group);

    win.add(&page);
    win.present();
}
