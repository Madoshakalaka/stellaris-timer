#![feature(fs_try_exists)]

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};
use std::fs::File;
use std::io::{BufReader, Cursor};
use std::ops::{Add, Deref};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use image::{ImageBuffer, ImageOutputFormat, Rgb, Rgba};
use leptess::{LepTess, Variable};
use leptess::capi::TessPageSegMode_PSM_SINGLE_LINE;
use regex::{Captures, Regex};
use tracing::{debug, info};
use tracing_subscriber;
use xcb::Connection;
use xcb::x::{Drawable, GetImage, ImageFormat, Screen};
use eframe::App;
use eframe::egui::{CentralPanel, Color32, Context, Frame, RichText, Slider, TextEdit, TextStyle};
use tokio::sync::{Notify};
use std::sync::RwLock;
use rodio::{Decoder, OutputStream, OutputStreamHandle, Source};
use tap::{Pipe, Tap};
use derive_more::{Deref, DerefMut};
use serde::{Deserialize, Deserializer, Serialize, Serializer};


#[derive(Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Date {
    year: u16,
    month: u8,
    day: u8,
}

impl Default for Date {
    fn default() -> Self {
        Self { year: 2200, month: 1, day: 1 }
    }
}


impl PartialOrd<Self> for Date {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Date {
    fn cmp(&self, other: &Self) -> Ordering {
        self.days_since_jesus().cmp(&other.days_since_jesus())
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StampedDate {
    time: Duration,
    date: Date,
}

impl Date {
    fn days_since_jesus(&self) -> u32 {
        self.year as u32 * 360 + self.month as u32 * 30 + self.day as u32
    }
}

impl PartialOrd<Self> for StampedDate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for StampedDate {
    fn cmp(&self, other: &Self) -> Ordering {
        let in_game_ordering = self.date.cmp(&other.date);
        match in_game_ordering {
            Ordering::Equal => {
                self.time.cmp(&other.time)
            }
            o => {
                o
            }
        }
    }
}

impl Date {
    fn with_days_added(&self, days: u16) -> Self {
        let day = self.day as u16 + days;
        let month_added = (day - 1) / 30;
        let day = ((day - 1) % 30 + 1) as u8;

        let month = month_added + self.month as u16 - 1;
        let year_added = month / 12;
        let month = (month % 12 + 1) as u8;

        let year = self.year + year_added;

        Self {
            year,
            month,
            day,
        }
    }
}


impl Display for Date {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.year, self.month, self.day)
    }
}

// yyyy.mm.dd
// min: column 2148 row 6
// max: column 2489 row 19

async fn capture(notify: Arc<Notify>, d: Arc<RwLock<Date>>) {
    let (conn, index) = Connection::connect(None).unwrap();

    let setup = conn.get_setup();
    let screen = setup.roots().nth(index as usize).unwrap();

    let matcher = Regex::new(r"^(\d{4}).(\d{2}).(\d{2})$").unwrap();

    'outer: loop {
        if let Some(new_d) = recognize_date(&conn, screen, &matcher) {
            let mut d = d.write().unwrap();
            *d = new_d;
        }
        let sleep = tokio::time::sleep(Duration::from_secs(1));
        let notified = notify.notified();
        tokio::select!(
            _ = sleep => {
            }
            _ = notified => {
                break 'outer
            }
        )
    }
}

fn recognize_date(conn: &Connection, screen: &Screen, matcher: &Regex) -> Option<Date> {
    const WIDTH: u16 = 75;
    const HEIGHT: u16 = 13;

    let get_image_cookie = conn.send_request(&GetImage {
        format: ImageFormat::ZPixmap,
        drawable: Drawable::Window(screen.root()),
        x: 2415,
        y: 6,
        width: WIDTH,
        height: HEIGHT,
        plane_mask: u32::MAX,
    });

    let get_image_reply = conn.wait_for_reply(get_image_cookie).unwrap();
    let mut bytes = Vec::from(get_image_reply.data());

    bytes.iter_mut().skip(3).step_by(4).for_each(|x| {
        *x = 255
    });
    let image: ImageBuffer<Rgba<u8>, _> = ImageBuffer::from_vec(WIDTH as u32, HEIGHT as u32, bytes).unwrap();

    let mut image_buffer = Vec::new();
    image.write_to(
        &mut Cursor::new(&mut image_buffer),
        ImageOutputFormat::Png,
    )
        .unwrap();

    let mut lt = LepTess::new(None, "eng").unwrap();
    lt.set_variable(Variable::TesseditPagesegMode, &*TessPageSegMode_PSM_SINGLE_LINE.to_string()).unwrap();
    lt.set_variable(Variable::TesseditCharWhitelist, "0123456789.").unwrap();
    lt.set_image_from_mem(&image_buffer).unwrap();
    let res = lt.get_utf8_text().unwrap();
    let res = res.trim();

    let caps = matcher.captures(res)?;
    parse_dt(caps)
}

fn parse_dt(res: Captures) -> Option<Date> {
    let y = (&res[1]).parse().map_err(|e| {
        debug!("{e}");
        e
    }).ok()?;
    let m = (&res[2]).parse().ok()?;
    let d = (&res[3]).parse().ok()?;

    if y < 3000 && y > 2199 && m < 13 && d < 31 {
        debug!("{y}, {m}, {d}");
        Some(Date { year: y, month: m, day: d })
    } else {
        None
    }
}


#[derive(Deref, DerefMut, Default)]
struct Reminders(BTreeMap<StampedDate, (String, bool)>);

impl Serialize for Reminders {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error> where S: Serializer {
        let v: Vec<_> = self.iter().clone().collect();
        v.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Reminders {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error> where D: Deserializer<'de> {
        let v = <Vec<_> as Deserialize>::deserialize(deserializer)?;
        Ok(Self(v.into_iter().collect()))
    }
}

struct MyApp {
    notify: Arc<Notify>,
    date: Arc<RwLock<Date>>,
    interval: String,
    reminders: Reminders,
    reminder_text: String,
    sound: &'static [u8],
    stream_handle: OutputStreamHandle,
    removing: Vec<StampedDate>,
    timer_file: PathBuf,
}

impl App for MyApp {
    fn update(&mut self, ctx: &Context, frame: &mut eframe::Frame) {
        ctx.request_repaint();
        CentralPanel::default().show(ctx, |ui| {
            ui.heading(self.date.read().unwrap().to_string());

            ui.horizontal(|ui| {
                ui.text_edit_singleline(&mut self.reminder_text);

                TextEdit::singleline(&mut self.interval).desired_width(100f32).show(ui);
                ui.label("days");
                let button = ui.button("Add");

                if button.clicked() {
                    if let Ok(interval) = meval::eval_str(&self.interval) {
                        if interval >= 1f64 && interval <= 300f64 * 360f64 {
                            let new_date = self.date.read().unwrap().with_days_added(interval as u16);
                            self.reminders.insert(StampedDate { time: SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap(), date: new_date }, (self.reminder_text.clone(), false));
                            self.dump();
                        }
                    };
                }
            });

            for (date, (reminder_text, highlighted)) in self.reminders.iter_mut() {
                let heading = RichText::new(&*reminder_text).heading();
                if !*highlighted && date.date.le(&*self.date.read().unwrap()) {
                    self.stream_handle.play_raw(Decoder::new(Cursor::new(self.sound.clone())).unwrap().convert_samples()).ok();
                    *highlighted = true
                }


                Frame::group(&*ctx.style()).fill(if *highlighted { Color32::BLUE } else { ui.visuals().widgets.inactive.bg_fill })
                    .show(ui, |ui| {
                        let mut r = ui.available_rect_before_wrap();
                        // r.set_height(ui.text_style_height(&TextStyle::Heading) * 3.0);

                        let mut left_r = r.clone();
                        left_r.max.x -= 40.0;

                        let mut right_r = r;
                        right_r.min.x = left_r.max.x;

                        let resp = ui.allocate_ui_at_rect(left_r, |ui| {
                            ui.label(heading);
                            ui.label(date.date.to_string());
                        });

                        right_r.max.y = resp.response.rect.max.y;

                        ui.allocate_ui_at_rect(right_r, |ui| {
                            ui.centered_and_justified(|ui| {
                                if ui.button("Clear").clicked() {
                                    self.removing.push(*date);
                                };
                            })
                        });
                    });
            }

            if !self.removing.is_empty() {
                self.removing.drain(..).for_each(|d| {
                    self.reminders.remove(&d);
                });
                self.dump();
            }
        });
    }

    fn on_close_event(&mut self) -> bool {
        self.dump();
        self.notify.notify_one();
        true
    }
}

impl MyApp {
    fn dump(&self) {
        let f = File::create(&self.timer_file).unwrap();
        let date = self.date.read().unwrap();
        let date = date.deref();
        serde_json::to_writer(f, &(date, &self.reminders)).unwrap();
    }
}


#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let notify = Arc::new(Notify::new());


    let timer_file = home::home_dir().unwrap().join(".stellaris-timer");

    let e = std::fs::try_exists(&timer_file).unwrap();
    let (date, reminders) = e.then(|| {
        serde_json::from_reader(File::open(&timer_file).unwrap()).ok()
    }).flatten()
        .unwrap_or_default();
// Date { year: 2200, month: 1, day: 1 }
    let date: Arc<RwLock<Date>> = Arc::new(RwLock::new(date));

    let ocr = tokio::spawn(capture(notify.clone(), date.clone()));

    let (_stream, stream_handle) = OutputStream::try_default().unwrap();
    let sound = include_bytes!("notif.wav");


    // info!("{timer_file} doesn't exist, creating");
    // File::create(timer_file).unwrap()

    // let sound = Decoder::new(file).unwrap();

    let options = eframe::NativeOptions::default();
    eframe::run_native(
        "Stellaris Timer",
        options,
        Box::new(move |_cc| {
            Box::new(MyApp {
                notify,
                date,
                interval: String::from(""),
                reminders,
                reminder_text: String::from(""),
                sound,
                stream_handle,
                removing: vec![],
                timer_file,
            })
        }),
    );

    tokio::join!(ocr);
}