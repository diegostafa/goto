// #![allow(unused)]
#![allow(clippy::identity_op)]

use fontdue::layout::{
    CoordinateSystem, HorizontalAlign, Layout, LayoutSettings, TextStyle, VerticalAlign, WrapStyle,
};
use fontdue::{Font, FontSettings, Metrics};
use std::collections::HashMap;
use std::error::Error;
use std::fmt::Display;
use std::fs::read_to_string;
use std::path::PathBuf;
use std::str::FromStr;
use x11rb::atom_manager;
use x11rb::connection::Connection;
use x11rb::protocol::render::{self, ConnectionExt as _, PictType};
use x11rb::protocol::xinput::{DeviceId, XIEventMask};
use x11rb::protocol::xproto::{ConnectionExt as _, *};
use x11rb::protocol::{Event, xinput};
use x11rb::resource_manager::Database;
use x11rb::wrapper::ConnectionExt;
use xkbcommon::xkb::{Keysym, keysym_from_name};

#[allow(unused)]
macro_rules! log_time {
    ($fn_call:expr) => {{
        let start = std::time::Instant::now();
        let result = $fn_call;
        let elapsed = start.elapsed();
        println!("Took: {:?}", elapsed);
        result
    }};
}

// --- main
const APP_NAME: &str = "goto";
const HICOLOR: &str = "/usr/share/icons/hicolor";
const INCH_TO_MM: f32 = 25.4;

type Result<T, E = Box<dyn Error>> = std::result::Result<T, E>;

fn main() -> Result<()> {
    let (conn, screen_num) = &x11rb::connect(None).expect("Failed to connect to X server");
    let res_db = x11rb::resource_manager::new_from_default(conn)?;
    let screen = &conn.setup().roots[*screen_num];
    conn.change_window_attributes(
        screen.root,
        &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
    )?;
    let (depth, visual) = choose_visual(conn, *screen_num)?;
    let atoms = &AtomCollection::new(conn)?.reply()?;
    let conf = &Config::new(screen, &res_db);
    let kb = Keys::init(conn, screen, conf)?;
    let mut tasks = TaskList::new();
    let wids = get_windows(conn, screen, atoms).unwrap_or_default();
    tasks.diff_update(wids, conn, atoms);
    if let Ok(Some(wid)) = get_active_window(conn, screen, atoms) {
        tasks.focus_by_wid(wid)
    }
    let icons = &mut IconCache::new();
    if conf.show_icons {
        icons.cache(conn, atoms, &tasks);
    }
    let mut geometry =
        compute_window_geometry(conf, screen, tasks.len()).unwrap_or((0.0, 0.0, 1.0, 1.0).into());
    let this_window = create_window(conn, screen, atoms, geometry, depth, visual)?;
    let mut frame = Frame::new(geometry.w as u32, geometry.h as u32);
    let gc = create_graphic_context(conn, this_window)?;

    let tr = &mut TextRenderer::new(conf);
    let mut is_mapped = false;
    let this_window_conf = ConfigureWindowAux::new().stack_mode(StackMode::ABOVE);

    macro_rules! show {
        () => {
            if !is_mapped {
                conn.configure_window(this_window, &this_window_conf)?;
                conn.map_window(this_window)?;
                is_mapped = true;
            }
        };
    }
    macro_rules! hide {
        () => {
            if is_mapped {
                conn.unmap_window(this_window)?;
                is_mapped = false;
            }
        };
    }
    loop {
        let mut title_changed = false;
        let mut icons_changed = false;
        let mut size_changed = false;
        let mut focus_changed = false;
        let mut window_changed = false;

        conn.flush()?;
        let event = conn.wait_for_event()?;
        let mut event_option = Some(event);
        while let Some(event) = event_option {
            match event {
                Event::Expose(_) => window_changed |= true,
                Event::Error(e) => {
                    if e.request_name == Some("GrabKey") {
                        eprintln!();
                        return Err(
                            "failed to grab keys, another program is probably grabbing them".into(),
                        );
                    }
                    println!("[WARNING] {e:?}")
                }
                Event::PropertyNotify(e) => {
                    if e.atom == atoms._NET_CLIENT_LIST {
                        if let Ok(wids) = get_windows(conn, screen, atoms) {
                            let before_len = tasks.len();
                            tasks.diff_update(wids, conn, atoms);
                            size_changed |= before_len != tasks.len();
                            focus_changed |= true;
                            if conf.show_icons {
                                icons.cache(conn, atoms, &tasks);
                                icons_changed |= true;
                            }
                        }
                    } else if e.atom == atoms._NET_ACTIVE_WINDOW {
                        if let Ok(wid) = get_active_window(conn, screen, atoms) {
                            match wid {
                                Some(wid) => {
                                    tasks.focus_by_wid(wid);
                                    focus_changed |= true;
                                }
                                None => {
                                    tasks.unfocus();
                                }
                            }
                        }
                    } else if (e.atom == atoms._NET_WM_NAME || e.atom == atoms.WM_NAME)
                        && let Ok(title) = get_window_title(conn, atoms, e.window)
                    {
                        tasks.update_title(e.window, title);
                        title_changed |= true;
                    } else if (e.atom == atoms._NET_WM_ICON)
                        && conf.show_icons
                        && let Some(task) = tasks.get_task_by_id(e.window)
                    {
                        icons.refresh(conn, atoms, task);
                        icons_changed |= true;
                    }
                }
                Event::XinputKeyRelease(e) => {
                    if e.detail == kb.key_mod.into() && is_mapped {
                        hide!();
                        if let Some(task) = tasks.selected()
                            && request_window_focus(conn, screen, atoms, task.wid).is_ok()
                        {
                            tasks.focus_by_selection();
                        }
                    }
                }
                Event::KeyPress(e) => {
                    if e.state & kb.modifier.bits() != KeyButMask::from(0u16) {
                        if e.detail == kb.key_next {
                            tasks.select_older();
                            focus_changed |= true;
                            show!();
                        } else if e.detail == kb.key_prev {
                            tasks.select_newer();
                            focus_changed |= true;
                            show!();
                        } else if e.detail == kb.key_kill && is_mapped {
                            if let Some(t) = tasks.selected()
                                && request_window_close(conn, atoms, t.wid).is_ok()
                            {
                                focus_changed |= true;
                                size_changed |= true;
                            }
                        } else if e.detail == kb.key_quit && is_mapped {
                            if let Ok(Some(_)) = get_active_window(conn, screen, atoms) {
                                tasks.select_end();
                            } else {
                                tasks.unfocus();
                            }
                            hide!();
                        }
                    }
                }
                _ => {}
            }
            event_option = conn.poll_for_event()?;
        }

        if size_changed {
            let Some(g) = compute_window_geometry(conf, screen, tasks.len()) else {
                hide!();
                continue;
            };

            geometry = g;
            request_window_move(conn, this_window, geometry)?;
            frame.resize(geometry.w as u32, geometry.h as u32);
            window_changed = true;
        }
        if is_mapped
            && !tasks.is_empty()
            && (focus_changed || title_changed || icons_changed || window_changed)
        {
            draw_list(&mut frame, conf, &tasks, tr, icons);
            send_frame(conn, this_window, gc, &frame, depth)?;
        }
    }
}

// --- config
#[derive(Debug)]
enum ListLayout {
    Rows,
    Columns,
}
#[derive(Debug, Copy, Clone)]
enum Size {
    Absolute(u32),
    Relative(f32),
}
impl Size {
    fn resolve(&self, dim: f32) -> f32 {
        match self {
            Size::Absolute(n) => *n as f32,
            Size::Relative(n) => n * dim,
        }
    }
}
#[derive(Debug)]
enum WindowLocation {
    NorthWest,
    North,
    NorthEast,
    West,
    Center,
    East,
    SouthWest,
    South,
    SouthEast,
}
impl WindowLocation {
    fn resolve(&self, (aw, ah): (f32, f32), (bw, bh): (f32, f32)) -> (f32, f32) {
        let half_aw = aw / 2.0;
        let half_ah = ah / 2.0;
        let half_bw = bw / 2.0;
        let half_bh = bh / 2.0;
        match self {
            WindowLocation::NorthWest => (0.0, 0.0),
            WindowLocation::North => (half_bw - half_aw, 0.0),
            WindowLocation::NorthEast => (bw - aw, 0.0),
            WindowLocation::West => (0.0, half_bh - ah),
            WindowLocation::Center => (half_bw - half_aw, half_bh - half_ah),
            WindowLocation::East => (bw - aw, half_bh - ah),
            WindowLocation::SouthWest => (0.0, bh - ah),
            WindowLocation::South => (half_bw - half_aw, bh - ah),
            WindowLocation::SouthEast => (bw - aw, bh - ah),
        }
    }
}
struct TaskStyle<'a> {
    bg_color: &'a Color,
    fg_color: &'a Color,
    border_color: &'a Color,
    border_width: f32,
}
struct Config {
    font_1: Option<PathBuf>,
    font_2: Option<PathBuf>,
    font_3: Option<PathBuf>,
    font_size: f32,
    text_halign: HorizontalAlign,
    text_valign: VerticalAlign,
    line_height: f32,
    show_marker: bool,
    marker: char,
    marker_fg_color: Color,
    marker_bg_color: Color,
    marker_width: Option<f32>,
    show_icons: bool,
    icon_padding: Size,
    icon_border_width: f32,
    icon_border_color: Color,
    icon_bg_color: Color,
    layout: ListLayout,
    location: WindowLocation,
    bg_color: Color,
    border_color: Color,
    border_width: f32,
    width: f32,
    height: f32,
    col_sep_width: f32,
    col_sep_color: Color,
    row_sep_width: f32,
    row_sep_color: Color,
    task_height: Size,
    task_width: Size,
    task_bg_color: Color,
    task_fg_color: Color,
    task_border_color: Color,
    task_border_width: f32,
    task_gradient: bool,
    selected_task_bg_color: Color,
    selected_task_fg_color: Color,
    selected_task_border_color: Color,
    selected_task_border_width: f32,
    key_quit: Keysym,
    key_next: Keysym,
    key_prev: Keysym,
    key_kill: Keysym,
    key_mod: Keysym,
}
impl Config {
    fn new(screen: &Screen, res_db: &Database) -> Self {
        let mut this = Self {
            font_1: None,
            font_2: None,
            font_3: None,
            font_size: 11.0,
            line_height: 1.1,
            text_halign: HorizontalAlign::Center,
            text_valign: VerticalAlign::Middle,
            show_marker: true,
            marker: '•',
            marker_width: Some(10.0),
            marker_fg_color: Color::new(255, 255, 255, 255),
            marker_bg_color: Color::new(0, 0, 0, 255),
            show_icons: true,
            icon_padding: Size::Relative(0.2),
            icon_border_width: 1.0,
            icon_border_color: Color::new(0, 0, 0, 255),
            icon_bg_color: Color::new(0, 0, 0, 255),
            layout: ListLayout::Rows,
            location: WindowLocation::Center,
            bg_color: Color::new(0, 0, 0, 255),
            border_color: Color::new(64, 64, 64, 255),
            border_width: 1.0,
            col_sep_width: 0.0,
            col_sep_color: Color::new(64, 64, 64, 255),
            row_sep_width: 0.0,
            row_sep_color: Color::new(64, 64, 64, 255),
            task_height: Size::Absolute(64),
            task_width: Size::Absolute(200),
            width: Size::Relative(0.4).resolve(screen.width_in_pixels as f32),
            height: Size::Relative(0.2).resolve(screen.width_in_pixels as f32),
            task_bg_color: Color::new(50, 50, 50, 255),
            task_fg_color: Color::new(255, 255, 255, 255),
            task_border_color: Color::new(200, 200, 200, 255),
            task_border_width: 0.0,
            task_gradient: true,
            selected_task_bg_color: Color::new(92, 64, 64, 255),
            selected_task_fg_color: Color::new(255, 255, 255, 255),
            selected_task_border_color: Color::new(128, 64, 32, 255),
            selected_task_border_width: 4.0,
            key_quit: Keysym::Escape,
            key_next: Keysym::Tab,
            key_prev: Keysym::backslash,
            key_kill: Keysym::K,
            key_mod: Keysym::Alt_L,
        };
        let dpi = get_dpi(res_db, screen).unwrap();
        this.font_size = apply_dpi(this.font_size, dpi);
        this.load_user_config(screen, dpi);
        this
    }
    fn load_user_config(&mut self, screen: &Screen, dpi: f32) {
        let Some(config_path) = Self::config_path() else {
            println!(
                "[INFO] `$XDG_CONFIG_HOME` and `$HOME` are not set, using default configuration"
            );
            return;
        };
        let Ok(file) = read_to_string(&config_path) else {
            println!("[INFO] failed to load `{config_path:?}`, using default configuration");
            return;
        };

        for (i, line) in file.lines().map(str::trim).enumerate() {
            macro_rules! warning {
                ($e:expr) => {
                    println!("[WARNING] line {}, failed to parse `{line}`: {}", i + 1, $e)
                };
            }
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, val)) = line.split_once(':') else {
                warning!("the format must be `key: value`");
                continue;
            };
            macro_rules! parse_assign {
                ($parser:ident, $field:ident) => {
                    match $parser(val) {
                        Ok(v) => self.$field = v,
                        Err(e) => warning!(e),
                    }
                };
            }
            macro_rules! parse_assign_font {
                ($field:ident) => {
                    match str_to_font_path(val) {
                        Ok(v) => self.$field = Some(v),
                        Err(e) => warning!(e),
                    }
                };
            }
            macro_rules! parse_assign_size {
                ($field:ident, $size:expr) => {
                    match str_to_size(val) {
                        Ok(val) => self.$field = val.resolve($size as f32),
                        Err(e) => warning!(e),
                    }
                };
            }
            match key.trim() {
                "font_size" => {
                    parse_assign!(str_to_primitive, font_size);
                    self.font_size = apply_dpi(self.font_size, dpi);
                }
                "font_1" => parse_assign_font!(font_1),
                "font_2" => parse_assign_font!(font_2),
                "font_3" => parse_assign_font!(font_3),
                "line_height" => parse_assign!(str_to_primitive, line_height),
                "text_halign" => parse_assign!(str_to_halign, text_halign),
                "text_valign" => parse_assign!(str_to_valign, text_valign),
                "show_marker" => parse_assign!(str_to_primitive, show_marker),
                "marker" => parse_assign!(str_to_primitive, marker),
                "marker_width" => parse_assign!(str_to_some_primitive, marker_width),
                "marker_fg_color" => parse_assign!(str_to_color, marker_fg_color),
                "marker_bg_color" => parse_assign!(str_to_color, marker_bg_color),
                "show_icons" => parse_assign!(str_to_primitive, show_icons),
                "icon_padding" => parse_assign!(str_to_size, icon_padding),
                "icon_border_width" => parse_assign!(str_to_primitive, icon_border_width),
                "icon_border_color" => parse_assign!(str_to_color, icon_border_color),
                "icon_bg_color" => parse_assign!(str_to_color, icon_bg_color),
                "layout" => parse_assign!(str_to_list_layout, layout),
                "location" => parse_assign!(str_to_position, location),
                "bg_color" => parse_assign!(str_to_color, bg_color),
                "border_color" => parse_assign!(str_to_color, border_color),
                "border_width" => parse_assign!(str_to_primitive, border_width),
                "task_height" => parse_assign!(str_to_size, task_height),
                "task_width" => parse_assign!(str_to_size, task_width),
                "width" => parse_assign_size!(width, screen.width_in_pixels),
                "height" => parse_assign_size!(height, screen.height_in_pixels),
                "col_sep_width" => parse_assign!(str_to_primitive, col_sep_width),
                "col_sep_color" => parse_assign!(str_to_color, col_sep_color),
                "row_sep_width" => parse_assign!(str_to_primitive, row_sep_width),
                "row_sep_color" => parse_assign!(str_to_color, row_sep_color),
                "task_bg_color" => parse_assign!(str_to_color, task_bg_color),
                "task_fg_color" => parse_assign!(str_to_color, task_fg_color),
                "task_border_width" => parse_assign!(str_to_primitive, task_border_width),
                "task_border_color" => parse_assign!(str_to_color, task_border_color),
                "task_gradient" => parse_assign!(str_to_primitive, task_gradient),
                "selected_task_bg_color" => {
                    parse_assign!(str_to_color, selected_task_bg_color)
                }
                "selected_task_fg_color" => {
                    parse_assign!(str_to_color, selected_task_fg_color)
                }
                "selected_task_border_color" => {
                    parse_assign!(str_to_color, selected_task_border_color)
                }
                "selected_task_border_width" => {
                    parse_assign!(str_to_primitive, selected_task_border_width)
                }
                "key_quit" => parse_assign!(str_to_keysym, key_quit),
                "key_next" => parse_assign!(str_to_keysym, key_next),
                "key_prev" => parse_assign!(str_to_keysym, key_prev),
                "key_kill" => parse_assign!(str_to_keysym, key_kill),
                "key_mod" => parse_assign!(str_to_keysym, key_mod),
                _ => warning!(format!("unknown key: `{key}`")),
            }
        }
        if self.font_1.is_none() && self.font_2.is_none() && self.font_3.is_none() {
            self.font_1 = Some(PathBuf::from("/usr/share/fonts/noto/NotoSans-Regular.ttf"));
        }
    }
    fn task_style(&self) -> TaskStyle<'_> {
        TaskStyle {
            fg_color: &self.task_fg_color,
            bg_color: &self.task_bg_color,
            border_color: &self.task_border_color,
            border_width: self.task_border_width,
        }
    }
    fn selected_task_style(&self) -> TaskStyle<'_> {
        TaskStyle {
            fg_color: &self.selected_task_fg_color,
            bg_color: &self.selected_task_bg_color,
            border_color: &self.selected_task_border_color,
            border_width: self.selected_task_border_width,
        }
    }
    fn config_path() -> Option<PathBuf> {
        if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
            return Some(PathBuf::from(xdg).join(format!("{APP_NAME}/config")));
        }
        if let Ok(home) = std::env::var("HOME") {
            return Some(PathBuf::from(home).join(format!(".config/{APP_NAME}/config")));
        }
        None
    }
}
fn str_to_primitive<T>(value: &str) -> Result<T, String>
where
    T: FromStr,
    T::Err: Display,
{
    let value = value.trim();
    if value.is_empty() {
        return Err("missing value".into());
    }
    value.parse::<T>().map_err(|e| e.to_string())
}
fn str_to_some_primitive<T>(value: &str) -> Result<Option<T>, String>
where
    T: FromStr,
    T::Err: Display,
{
    let value = value.trim();
    if value.is_empty() {
        return Err("missing value".into());
    }
    match value.to_lowercase().as_str() {
        "auto" => Ok(None),
        val => str_to_primitive(val).map(Some),
    }
}
fn str_to_size(value: &str) -> Result<Size> {
    let value = value.trim();
    if value.is_empty() {
        return Err("missing value".into());
    }
    if value.ends_with('%') {
        return match value[0..value.len() - 1].trim_end().parse::<f32>() {
            Ok(n) => Ok(Size::Relative(n / 100.0)),
            Err(e) => Err(e.into()),
        };
    }
    match value[0..value.len()].trim_end().parse::<u32>() {
        Ok(n) => Ok(Size::Absolute(n)),
        Err(e) => Err(e.into()),
    }
}
fn str_to_position(value: &str) -> Result<WindowLocation> {
    let value = value.trim();
    if value.is_empty() {
        return Err("missing value".into());
    }
    match value.to_lowercase().as_str() {
        "1" => Ok(WindowLocation::NorthWest),
        "2" => Ok(WindowLocation::North),
        "3" => Ok(WindowLocation::NorthEast),
        "4" => Ok(WindowLocation::West),
        "5" => Ok(WindowLocation::Center),
        "6" => Ok(WindowLocation::East),
        "7" => Ok(WindowLocation::SouthWest),
        "8" => Ok(WindowLocation::South),
        "9" => Ok(WindowLocation::SouthEast),
        _ => Err(format!(
            "invalid location `{value}`, expected a value between 1 (top left) and 9 (bottom right)"
        )
        .into()),
    }
}
fn str_to_color(value: &str) -> Result<Color> {
    let value = value.trim();
    if value.is_empty() {
        return Err("missing value".into());
    }
    if &value[0..1] != "#" {
        return Err("a color must start with `#`".into());
    }
    let value = &value[1..];
    if value.len() == 3 {
        let r = u8::from_str_radix(&value[0..1].repeat(2), 16).map_err(|e| e.to_string())?;
        let g = u8::from_str_radix(&value[1..2].repeat(2), 16).map_err(|e| e.to_string())?;
        let b = u8::from_str_radix(&value[2..3].repeat(2), 16).map_err(|e| e.to_string())?;
        return Ok(Color::new(r, g, b, 255));
    }
    if value.len() == 6 {
        let r = u8::from_str_radix(&value[0..2], 16).map_err(|e| e.to_string())?;
        let g = u8::from_str_radix(&value[2..4], 16).map_err(|e| e.to_string())?;
        let b = u8::from_str_radix(&value[4..6], 16).map_err(|e| e.to_string())?;
        return Ok(Color::new(r, g, b, 255));
    }
    if value.len() == 8 {
        let r = u8::from_str_radix(&value[0..2], 16).map_err(|e| e.to_string())?;
        let g = u8::from_str_radix(&value[2..4], 16).map_err(|e| e.to_string())?;
        let b = u8::from_str_radix(&value[4..6], 16).map_err(|e| e.to_string())?;
        let a = u8::from_str_radix(&value[6..8], 16).map_err(|e| e.to_string())?;
        return Ok(Color::new(r, g, b, a));
    }
    Err(
        format!("invalid hex color `{value}`, valid formats: `#rgb`, `#rrggbb`, `#rrggbbaa`")
            .into(),
    )
}
fn str_to_keysym(value: &str) -> Result<Keysym> {
    let value = value.trim();
    if value.is_empty() {
        return Err("missing value".into());
    }
    let sym = keysym_from_name(value, 0);
    if sym == Keysym::NoSymbol {
        return Err(format!("invalid keysym `{value}`").into());
    }
    Ok(sym)
}
fn str_to_font_path(value: &str) -> Result<PathBuf> {
    let value = value.trim();
    if value.is_empty() {
        return Err("missing value".into());
    }
    let path = PathBuf::from(value);
    if !path.exists() {
        return Err(format!("couldn't find font `{value}`").into());
    }
    Ok(path)
}
fn str_to_halign(value: &str) -> Result<HorizontalAlign> {
    let value = value.trim();
    if value.is_empty() {
        return Err("missing value".into());
    }
    match value.to_lowercase().as_str() {
        "left" => Ok(HorizontalAlign::Left),
        "center" => Ok(HorizontalAlign::Center),
        "right" => Ok(HorizontalAlign::Right),
        _ => Err(
            format!("invalid alignment: `{value}`, expecting: `left`, `center` or `right`").into(),
        ),
    }
}
fn str_to_valign(value: &str) -> Result<VerticalAlign> {
    let value = value.trim();
    if value.is_empty() {
        return Err("missing value".into());
    }
    match value.to_lowercase().as_str() {
        "top" => Ok(VerticalAlign::Top),
        "middle" => Ok(VerticalAlign::Middle),
        "bottom" => Ok(VerticalAlign::Bottom),
        _ => Err(
            format!("invalid alignment: `{value}`, expecting: `top`, `middle` or `bottom`").into(),
        ),
    }
}
fn str_to_list_layout(value: &str) -> Result<ListLayout> {
    let value = value.trim();
    if value.is_empty() {
        return Err("missing value".into());
    }
    match value.to_lowercase().as_str() {
        "rows" => Ok(ListLayout::Rows),
        "columns" => Ok(ListLayout::Columns),
        _ => Err(format!("invalid list layout: `{value}`, expecting: `rows`, `columns`").into()),
    }
}

// --- data
#[derive(Debug)]
struct Task {
    wid: Window,
    // pid: Option<u32>,
    title: String,
    class: (String, String),
}
impl PartialEq for Task {
    fn eq(&self, other: &Self) -> bool {
        self.wid == other.wid
    }
}
#[derive(Debug)]
struct TaskList {
    tasks: Vec<Task>,
    selected: Option<usize>,
}
impl TaskList {
    fn new() -> Self {
        Self {
            tasks: Vec::with_capacity(64),
            selected: None,
        }
    }
    fn selected(&self) -> Option<&Task> {
        self.selected.map(|sel| &self.tasks[sel])
    }
    fn get_task_by_id(&self, wid: Window) -> Option<&Task> {
        self.tasks.iter().find(|task| task.wid == wid)
    }
    fn list_ascending(&self) -> (impl Iterator<Item = &Task>, Option<usize>) {
        (self.tasks.iter(), self.selected)
    }
    fn list_descending(&self) -> (impl Iterator<Item = &Task>, Option<usize>) {
        (
            self.tasks.iter().rev(),
            self.selected.map(|sel| self.len() - 1 - sel),
        )
    }
    fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }
    fn len(&self) -> usize {
        self.tasks.len()
    }
    fn contains(&self, wid: Window) -> bool {
        self.tasks.iter().any(|task| task.wid == wid)
    }
    fn update_title(&mut self, wid: Window, title: String) {
        if let Some(task) = self.tasks.iter_mut().find(|task| task.wid == wid) {
            task.title = title;
        }
    }
    fn diff_update(&mut self, wids: Vec<Window>, conn: &impl Connection, atoms: &AtomCollection) {
        // untrack windows not in wids
        let mut old_wids = Vec::with_capacity(self.len());
        self.tasks
            .iter()
            .filter(|task| !wids.contains(&task.wid))
            .for_each(|task| old_wids.push(task.wid));
        old_wids.into_iter().for_each(|wid| self.untrack(wid));

        // track windows not in tasks
        let propmask = &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE);
        let mut new_wids = Vec::with_capacity(wids.len());
        wids.into_iter()
            .filter(|wid| !self.contains(*wid))
            .for_each(|wid| new_wids.push(wid));
        new_wids
            .into_iter()
            .filter_map(|wid| window_to_task(conn, atoms, wid))
            .for_each(|task| {
                let _ = conn.change_window_attributes(task.wid, propmask);
                self.track(task);
            });
    }
    fn track(&mut self, task: Task) {
        if !self.tasks.contains(&task) {
            self.tasks.push(task);
        }
    }
    fn untrack(&mut self, wid: Window) {
        self.tasks.retain(|task| task.wid != wid);
        if let Some(sel) = self.selected {
            if let Some(last) = self.len().checked_sub(1) {
                self.selected = Some(sel.min(last));
            } else {
                self.selected = None;
            }
        }
    }
    fn select_newer(&mut self) {
        if !self.is_empty() {
            if let Some(sel) = self.selected {
                self.selected = Some((sel + 1) % self.len());
            } else {
                let last = self.len().checked_sub(1);
                self.selected = last;
            }
        }
    }
    fn select_older(&mut self) {
        if !self.is_empty() {
            let last = self.len().checked_sub(1);
            if let Some(sel) = self.selected {
                self.selected = sel.checked_sub(1).or(last);
            } else {
                self.selected = last;
            }
        }
    }
    fn select_end(&mut self) {
        if !self.is_empty() {
            self.selected = self.len().checked_sub(1);
        }
    }
    fn focus_by_index(&mut self, idx: usize) {
        if idx < self.len() {
            let task = self.tasks.remove(idx);
            self.tasks.push(task);
            self.select_end();
        }
    }
    fn focus_by_selection(&mut self) {
        if let Some(sel) = self.selected {
            self.focus_by_index(sel);
        }
    }
    fn focus_by_wid(&mut self, wid: Window) {
        if let Some(idx) = self.tasks.iter().position(|task| task.wid == wid) {
            self.focus_by_index(idx);
        }
    }
    fn unfocus(&mut self) {
        self.selected = None;
    }
}

// --- gui
#[derive(Clone, Copy)]
struct Area {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}
impl Area {
    fn shrink(mut self, amount: f32) -> Self {
        self.x += amount;
        self.y += amount;
        self.w -= amount * 2.0;
        self.h -= amount * 2.0;
        self
    }
}
#[derive(Clone, Copy)]
struct Color {
    r: u8,
    g: u8,
    b: u8,
    a: u8,
}
impl Color {
    fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }
    fn multiply(&self, factor: f32) -> Self {
        Self {
            r: (self.r as f32 * factor) as u8,
            g: (self.g as f32 * factor) as u8,
            b: (self.b as f32 * factor) as u8,
            a: (self.a as f32 * factor) as u8,
        }
    }
    fn _from_rgba(color: u32) -> Self {
        Self {
            r: ((color >> 0) & 0xFF) as u8,
            g: ((color >> 8) & 0xFF) as u8,
            b: ((color >> 16) & 0xFF) as u8,
            a: ((color >> 24) & 0xFF) as u8,
        }
    }
    fn to_bgra(self) -> u32 {
        u32::from_ne_bytes([self.b, self.g, self.r, self.a])
    }
    fn _to_argb(self) -> u32 {
        u32::from_ne_bytes([self.a, self.r, self.g, self.b])
    }
    fn _to_rgba(self) -> u32 {
        u32::from_ne_bytes([self.r, self.g, self.b, self.a])
    }
}
#[derive(Clone)]
struct Frame {
    buf: Vec<u8>,
    width: u32,
    height: u32,
}
impl Frame {
    const CHANNELS: u32 = 4;

    fn new(width: u32, height: u32) -> Self {
        Self {
            buf: vec![0; (width * height * Self::CHANNELS) as usize],
            width,
            height,
        }
    }
    fn from_rgba_u8(buf: &[u8], width: u32, height: u32) -> Self {
        let mut frame = Self::new(width, height);
        let frame_buf = frame.buf_u32_mut();
        for (i, rgba) in buf.chunks(4).enumerate() {
            frame_buf[i] = u32::from_ne_bytes([rgba[2], rgba[1], rgba[0], rgba[3]]);
        }
        frame
    }
    fn from_argb_u32(buf: &[u32], width: u32, height: u32) -> Self {
        let mut frame = Self::new(width, height);
        for (i, argb) in buf.iter().enumerate() {
            frame.buf[i * 4 + 0] = ((*argb >> 0) & 0xFF) as u8;
            frame.buf[i * 4 + 1] = ((*argb >> 8) & 0xFF) as u8;
            frame.buf[i * 4 + 2] = ((*argb >> 16) & 0xFF) as u8;
            frame.buf[i * 4 + 3] = ((*argb >> 24) & 0xFF) as u8;
        }
        frame
    }
    fn resize(&mut self, width: u32, height: u32) {
        self.buf
            .resize((width * height * Self::CHANNELS) as usize, 0);
        self.width = width;
        self.height = height;
    }
    fn _scale_nn(&self, factor: f32) -> Self {
        let (src_width, src_height) = (self.width as usize, self.height as usize);
        let src_buf = self.buf_u32();

        let dst_width = (src_width as f32 * factor).round().max(1.0) as usize;
        let dst_height = (src_height as f32 * factor).round().max(1.0) as usize;

        let mut dst = Self::new(dst_width as u32, dst_height as u32);
        let dst_buf = dst.buf_u32_mut();

        for y in 0..dst_height {
            let src_y = (((y as f32) / factor).floor() as usize).min(src_height - 1);
            for x in 0..dst_width {
                let src_x = (((x as f32) / factor).floor() as usize).min(src_width - 1);
                dst_buf[y * dst_width + x] = src_buf[src_y * src_width + src_x];
            }
        }
        dst
    }
    fn scale_bilinear(&self, factor: f32) -> Self {
        let (src_width, src_height) = (self.width as usize, self.height as usize);
        let src_buf = self.buf_u32();

        let dst_width = (src_width as f32 * factor).round().max(1.0) as usize;
        let dst_height = (src_height as f32 * factor).round().max(1.0) as usize;

        let mut dst = Self::new(dst_width as u32, dst_height as u32);
        let dst_buf = dst.buf_u32_mut();

        let mut x_map = Vec::with_capacity(dst_width);
        let mut y_map = Vec::with_capacity(dst_height);

        for x in 0..dst_width {
            let src_x = (x as f32) * ((src_width - 1) as f32) / ((dst_width - 1).max(1) as f32);
            let x0 = src_x.floor() as usize;
            let x1 = (x0 + 1).min(src_width - 1);
            let dx = src_x - x0 as f32;
            x_map.push((x0, x1, dx));
        }

        for y in 0..dst_height {
            let src_y = (y as f32) * ((src_height - 1) as f32) / ((dst_height - 1).max(1) as f32);
            let y0 = src_y.floor() as usize;
            let y1 = (y0 + 1).min(src_height - 1);
            let dy = src_y - y0 as f32;
            y_map.push((y0, y1, dy));
        }

        for (y, &(y0, y1, dy)) in y_map.iter().enumerate() {
            let row0 = &src_buf[y0 * src_width..(y0 + 1) * src_width];
            let row1 = &src_buf[y1 * src_width..(y1 + 1) * src_width];

            for (x, &(x0, x1, dx)) in x_map.iter().enumerate() {
                let p00 = row0[x0];
                let p10 = row0[x1];
                let p01 = row1[x0];
                let p11 = row1[x1];

                let interp = |shift: u32| -> u32 {
                    let c00 = ((p00 >> shift) & 0xFF) as f32;
                    let c10 = ((p10 >> shift) & 0xFF) as f32;
                    let c01 = ((p01 >> shift) & 0xFF) as f32;
                    let c11 = ((p11 >> shift) & 0xFF) as f32;

                    let c0 = c00 * (1.0 - dx) + c10 * dx;
                    let c1 = c01 * (1.0 - dx) + c11 * dx;
                    ((c0 * (1.0 - dy) + c1 * dy).round() as u32) & 0xFF
                };

                let b = interp(0);
                let g = interp(8);
                let r = interp(16);
                let a = interp(24);

                dst_buf[y * dst_width + x] = (a << 24) | (r << 16) | (g << 8) | b;
            }
        }
        dst
    }
    fn width(&self) -> u32 {
        self.width
    }
    fn height(&self) -> u32 {
        self.height
    }
    fn buf_u8(&self) -> &[u8] {
        &self.buf
    }
    fn buf_u32(&self) -> &[u32] {
        unsafe {
            std::slice::from_raw_parts(
                self.buf.as_ptr() as *const u32,
                (self.width * self.height) as usize,
            )
        }
    }
    fn _buf_u8_mut(&mut self) -> &mut [u8] {
        &mut self.buf
    }
    fn buf_u32_mut(&mut self) -> &mut [u32] {
        unsafe {
            std::slice::from_raw_parts_mut(
                self.buf.as_mut_ptr() as *mut u32,
                (self.width * self.height) as usize,
            )
        }
    }
    fn blit_frame(&mut self, frame: &Frame, x: i32, y: i32) {
        let dst_width = self.width as usize;
        let dst_height = self.height as usize;
        let src_width = frame.width as usize;
        let src_height = frame.height as usize;

        let src = frame.buf_u32();
        let dst = self.buf_u32_mut();

        // Iterate over source rows
        for sy in 0..src_height {
            let dy = y + sy as i32;
            if dy < 0 || dy >= dst_height as i32 {
                continue; // skip out-of-bounds rows
            }

            let dst_row_start = dy as usize * dst_width;
            let src_row_start = sy * src_width;

            for sx in 0..src_width {
                let dx = x + sx as i32;
                if dx < 0 || dx >= dst_width as i32 {
                    continue; // skip out-of-bounds columns
                }

                let dst_idx = dst_row_start + dx as usize;
                let src_idx = src_row_start + sx;

                dst[dst_idx] = src[src_idx];
            }
        }
    }
    fn draw_rect(&mut self, area: Area, color: &Color) {
        let color = color.to_bgra();

        let x = area.x.floor() as u32;
        let y = area.y.floor() as u32;
        let w = area.w.ceil() as u32;
        let h = area.h.ceil() as u32;

        let width = self.width;
        let buf = self.buf_u32_mut();

        for row in y..y + h {
            let start = (row * width + x) as usize;
            let end = start + w as usize;
            buf[start..end].fill(color);
        }
    }
    fn draw_rect_outline(&mut self, area: Area, bw: f32, color: &Color) {
        if bw <= 0.0 {
            return;
        }

        let x = area.x;
        let y = area.y;
        let w = area.w;
        let h = area.h;

        let l = Area::from((x, y, bw, h));
        let t = Area::from((x, y, w, bw));
        let d = Area::from((x, y + h - bw, w, bw));
        let r = Area::from((x + w - bw, y, bw, h));

        self.draw_rect(l, color);
        self.draw_rect(t, color);
        self.draw_rect(r, color);
        self.draw_rect(d, color);
    }
    fn draw_hline(&mut self, width: f32, y: f32, x1: f32, x2: f32, color: &Color) {
        if width <= 0.0 {
            return;
        }
        let area = Area::from((x1, y, x2 - x1, width));
        self.draw_rect(area, color);
    }
    fn _draw_vline(&mut self, width: f32, x: f32, y1: f32, y2: f32, color: &Color) {
        if width <= 0.0 {
            return;
        }
        let area = Area::from((x, y1, width, y2 - y1));
        self.draw_rect(area, color);
    }
}

impl From<(f32, f32, f32, f32)> for Area {
    fn from(value: (f32, f32, f32, f32)) -> Self {
        Self {
            x: value.0,
            y: value.1,
            w: value.2,
            h: value.3,
        }
    }
}
type RasterizedGlyph = (Metrics, Vec<u8>);
struct TextRenderer {
    ascii: [(Metrics, Vec<u8>); 256],
    others: HashMap<char, RasterizedGlyph>,
    fonts: Vec<Font>,
    size: f32,
    layout: Layout,
}
impl TextRenderer {
    pub fn new(conf: &Config) -> Self {
        let font_paths: Vec<_> = vec![&conf.font_1, &conf.font_2, &conf.font_3]
            .into_iter()
            .flatten()
            .collect();

        let fonts: Vec<_> = font_paths
            .into_iter()
            .map(|font_path| {
                let font_bytes = std::fs::read(font_path).unwrap();
                Font::from_bytes(
                    font_bytes,
                    FontSettings {
                        scale: conf.font_size,
                        ..Default::default()
                    },
                )
                .unwrap()
            })
            .collect();

        let mut ascii: [RasterizedGlyph; 256] = std::array::from_fn(|_| RasterizedGlyph::default());
        let font = &fonts[0];
        for c in 0u8..=255 {
            ascii[c as usize] = Self::rasterize(c as char, font, conf.font_size);
        }

        Self {
            ascii,
            others: HashMap::new(),
            fonts,
            size: conf.font_size,
            layout: Layout::new(CoordinateSystem::PositiveYDown),
        }
    }
    pub fn get(&self, c: char) -> &RasterizedGlyph {
        self.ascii
            .get(c as usize)
            .or_else(|| self.others.get(&c))
            .unwrap()
    }
    fn set_layout(&mut self, text: &str, conf: &Config, area: Area) {
        for c in text.chars() {
            self.cache(c);
        }
        let mut settings = LayoutSettings {
            x: area.x,
            y: area.y,
            max_width: Some(area.w),
            max_height: Some(area.h),
            horizontal_align: conf.text_halign,
            vertical_align: conf.text_valign,
            wrap_style: WrapStyle::Word,
            wrap_hard_breaks: true,
            line_height: conf.line_height,
        };
        self.layout.reset(&settings);

        // fixme:
        // a rasterized glyph might not match its computed layout:
        // - layouts are all computed with a single font (index 0)
        // - the rasterized glyph is instead computed with the appropriate font
        self.layout
            .append(&self.fonts, &TextStyle::new(text, self.size, 0));

        if self.layout.height() > area.h {
            settings.vertical_align = VerticalAlign::Top;
            self.layout.reset(&settings);
            self.layout
                .append(&self.fonts, &TextStyle::new(text, self.size, 0));
        }
    }

    fn cache(&mut self, c: char) {
        if c.is_ascii() {
            return;
        }
        if self.others.contains_key(&c) {
            return;
        }
        if let Some(font) = self.font_for_char(c) {
            let (metrics, bitmap) = Self::rasterize(c, font, self.size);
            if bitmap.is_empty() {
                // likely an emoji that fontdue can't rasterize
                self.others.insert(c, Default::default());
                return;
            }
            self.others.insert(c, (metrics, bitmap));
            return;
        }
        println!("couldn't find a suitable font for `{c}`");
        self.others.insert(c, Default::default());
    }
    fn font_for_char(&self, c: char) -> Option<&Font> {
        self.fonts.iter().find(|font| font.has_glyph(c))
    }
    fn rasterize(c: char, font: &Font, size: f32) -> RasterizedGlyph {
        let (metrics, bitmap) = font.rasterize(c, size);
        (metrics, bitmap)
    }
}
fn draw_list(
    frame: &mut Frame,
    conf: &Config,
    tasks: &TaskList,
    tr: &mut TextRenderer,
    icons: &mut IconCache,
) {
    match conf.layout {
        ListLayout::Rows => draw_list_rows(frame, conf, tasks, tr, icons),
        ListLayout::Columns => draw_list_cols(frame, conf, tasks, tr, icons),
    }
}
fn draw_list_rows(
    frame: &mut Frame,
    conf: &Config,
    tasks: &TaskList,
    tr: &mut TextRenderer,
    icons: &mut IconCache,
) {
    let (list, Some(selected_idx)) = tasks.list_descending() else {
        return;
    };
    let mut area = Area::from((0.0, 0.0, frame.width() as f32, frame.height() as f32));
    frame.draw_rect(area, &conf.bg_color);
    frame.draw_rect_outline(area, conf.border_width, &conf.border_color);
    area = area.shrink(conf.border_width);

    let task_h = area.h / tasks.len() as f32;

    let icon_x = area.x;
    let icon_w = if conf.show_icons { task_h } else { 0.0 };

    let marker_w = if conf.show_marker {
        conf.marker_width.unwrap_or(task_h)
    } else {
        0.0
    };
    let marker_x = area.x + area.w - marker_w;

    let task_x = area.x + icon_w;
    let task_w = area.w - icon_w - marker_w;
    let style = conf.selected_task_style();

    for (i, task) in list.enumerate() {
        let y = area.y + task_h * i as f32;
        let is_selected = i == selected_idx;

        // left
        if conf.show_icons {
            let icon = icons.get(task);
            let icon_area = (icon_x, y, icon_w, icon_w);
            draw_icon(frame, conf, icon, icon_area.into());
        }

        // center
        let task_area = Area::from((task_x, y, task_w, task_h));
        if is_selected {
            draw_task(frame, conf, task, tr, &style, task_area);
        } else {
            let mut style = conf.task_style();
            let step = 1.0 - (i as f32 / tasks.len() as f32);
            let gradient = Color::new(
                (step * style.bg_color.r as f32) as u8,
                (step * style.bg_color.g as f32) as u8,
                (step * style.bg_color.b as f32) as u8,
                (step * style.bg_color.a as f32) as u8,
            );
            if conf.task_gradient {
                style.bg_color = &gradient;
            }
            draw_task(frame, conf, task, tr, &style, task_area);
        };

        // right
        if conf.show_marker {
            let marker_area = Area::from((marker_x, y, marker_w, task_h));
            // draw_rect(pm, &conf.marker_bg_color, marker_area.into());
            if is_selected {
                draw_marker(frame, conf, tr, marker_area);
            }
        }

        // row separator
        if i != 0 {
            frame.draw_hline(
                conf.row_sep_width,
                y,
                area.x,
                area.x + area.w,
                &conf.row_sep_color,
            );
        }
    }
}
fn draw_list_cols(
    frame: &mut Frame,
    conf: &Config,
    tasks: &TaskList,
    tr: &mut TextRenderer,
    icons: &mut IconCache,
) {
    let (list, Some(selected_idx)) = tasks.list_descending() else {
        return;
    };
    let mut area = Area::from((0.0, 0.0, frame.width() as f32, frame.height() as f32));
    frame.draw_rect(area, &conf.bg_color);
    frame.draw_rect_outline(area, conf.border_width, &conf.border_color);
    area = area.shrink(conf.border_width);

    let task_w = area.w / tasks.len() as f32;

    let icon_y = area.y;
    let icon_h = if conf.show_icons { task_w } else { 0.0 };

    let marker_h = if conf.show_marker {
        conf.marker_width.unwrap_or(task_w)
    } else {
        0.0
    };
    let marker_y = area.y + area.h - marker_h;

    let task_y = area.y + icon_h;
    let task_h = area.h - icon_h - marker_h;

    let style = conf.selected_task_style();

    for (i, task) in list.enumerate() {
        let x = area.x + task_w * i as f32;
        let is_selected = i == selected_idx;

        // left
        if conf.show_icons {
            let icon = icons.get(task);
            let icon_area = (x, icon_y, icon_h, icon_h);
            draw_icon(frame, conf, icon, icon_area.into());
        }

        // center
        let task_area = Area::from((x, task_y, task_w, task_h));
        if is_selected {
            draw_task(frame, conf, task, tr, &style, task_area);
        } else {
            let mut style = conf.task_style();
            let step = 1.0 - (i as f32 / tasks.len() as f32);
            let gradient = Color::new(
                (step * style.bg_color.r as f32) as u8,
                (step * style.bg_color.g as f32) as u8,
                (step * style.bg_color.b as f32) as u8,
                (step * style.bg_color.a as f32) as u8,
            );
            if conf.task_gradient {
                style.bg_color = &gradient;
            }
            draw_task(frame, conf, task, tr, &style, task_area);
        };

        // right
        if conf.show_marker {
            let marker_area = Area::from((x, marker_y, task_h, marker_h));
            // draw_rect(pm, &conf.marker_bg_color, marker_area.into());
            if is_selected {
                draw_marker(frame, conf, tr, marker_area);
            }
        }

        // row separator
        // if i != 0 {
        //     _draw_vline(
        //         pm,
        //         &conf.row_sep_color,
        //         conf.row_sep_width,
        //         y,
        //         area.x,
        //         area.x + area.w,
        //     );
        // }
    }
}
fn draw_marker(frame: &mut Frame, conf: &Config, tr: &mut TextRenderer, area: Area) {
    let mut buf = [0u8; 4];
    let marker_str = conf.marker.encode_utf8(&mut buf);
    tr.set_layout(marker_str, conf, area);
    frame.draw_rect(area, &conf.marker_bg_color);
    draw_text(frame, &conf.marker_fg_color, tr);
}
fn draw_icon(frame: &mut Frame, conf: &Config, icon: &Frame, mut area: Area) {
    frame.draw_rect(area, &conf.icon_bg_color);
    frame.draw_rect_outline(area, conf.icon_border_width, &conf.icon_border_color);

    area = area.shrink(conf.icon_border_width);
    area = area.shrink(conf.icon_padding.resolve(area.h));

    let factor = area.w / (icon.width().max(icon.height()) as f32);
    let scaled = icon.scale_bilinear(factor);
    frame.blit_frame(&scaled, area.x as i32, area.y as i32);
}
fn draw_task(
    frame: &mut Frame,
    conf: &Config,
    task: &Task,
    tr: &mut TextRenderer,
    style: &TaskStyle,
    area: Area,
) {
    frame.draw_rect(area, style.bg_color);
    frame.draw_rect_outline(area, style.border_width, style.border_color);

    let bw = conf.task_border_width.max(conf.selected_task_border_width);
    tr.set_layout(&task.title, conf, area.shrink(bw));
    draw_text(frame, style.fg_color, tr);
}
fn draw_text(frame: &mut Frame, color: &Color, tr: &TextRenderer) {
    let frame_width = frame.width() as usize;
    let frame = frame.buf_u32_mut();

    for glyph_pos in tr.layout.glyphs() {
        let (metrics, bitmap) = tr.get(glyph_pos.parent);
        for row in 0..metrics.height {
            for col in 0..metrics.width {
                let b_offset = row * metrics.width + col;
                let a = bitmap[b_offset] as f32 / 255.0;
                if a == 0.0 {
                    continue;
                }
                let px = (glyph_pos.x as usize) + col;
                let py = (glyph_pos.y as usize) + row;
                let p_offset = py * frame_width + px;
                if p_offset >= frame.len() {
                    continue;
                }
                frame[p_offset] = color.multiply(a).to_bgra();
            }
        }
    }
}

// --- x11
atom_manager! {
    AtomCollection: AtomCollectionCookie {
        ATOM,
        WM_PROTOCOLS,
        WM_DELETE_WINDOW,
        UTF8_STRING,
        WM_NAME,
        WM_CLASS,
        CARDINAL,
        STRING,
        WINDOW,
        WM_TRANSIENT_FOR,

        _NET_WM_PID,
        _NET_WM_STATE,
        _NET_WM_STATE_ABOVE,
        _NET_WM_NAME,
        _NET_WM_ICON,
        _NET_ACTIVE_WINDOW,
        _NET_CLIENT_LIST,
        _NET_WM_STATE_SKIP_TASKBAR,
        _NET_WM_WINDOW_TYPE,
        _NET_WM_WINDOW_TYPE_DIALOG,
    }
}
struct Keys {
    key_next: Keycode,
    key_prev: Keycode,
    key_kill: Keycode,
    key_quit: Keycode,
    key_mod: Keycode,
    modifier: ModMask,
}
impl Keys {
    fn init(conn: &impl Connection, screen: &Screen, conf: &Config) -> Result<Self> {
        let setup = conn.setup();
        let min_keycode = setup.min_keycode;
        let max_keycode = setup.max_keycode;
        let reply = conn
            .get_keyboard_mapping(min_keycode, max_keycode - min_keycode + 1)?
            .reply()?;
        let sym_to_code = |k: Keysym| {
            reply
                .keysyms
                .iter()
                .position(|&ks| ks == k.raw())
                .map(|i| (i / reply.keysyms_per_keycode as usize) as u8 + min_keycode)
                .unwrap()
        };

        let key_next = sym_to_code(conf.key_next);
        let key_prev = sym_to_code(conf.key_prev);
        let key_kill = sym_to_code(conf.key_kill);
        let key_quit = sym_to_code(conf.key_quit);
        let key_mod = sym_to_code(conf.key_mod);

        let map = conn.get_modifier_mapping()?.reply()?;
        let keycodes_per_mod = map.keycodes_per_modifier() as usize;
        let mut modifier = 0;
        for (mod_index, chunk) in map.keycodes.chunks(keycodes_per_mod).enumerate() {
            if chunk.contains(&key_mod) {
                modifier = 1 << mod_index;
                break;
            }
        }
        if modifier == 0 {
            return Err(format!("`{key_mod}` is not a modifier").into());
        }
        let modifier = ModMask::from(modifier as u16);
        let mode = GrabMode::ASYNC;
        conn.grab_key(false, screen.root, modifier, key_next, mode, mode)?;
        conn.grab_key(false, screen.root, modifier, key_prev, mode, mode)?;
        conn.grab_key(false, screen.root, modifier, key_kill, mode, mode)?;
        conn.grab_key(false, screen.root, modifier, key_quit, mode, mode)?;

        xinput::ConnectionExt::xinput_xi_select_events(
            conn,
            screen.root,
            &[xinput::EventMask {
                deviceid: DeviceId::from(0u16),
                mask: vec![XIEventMask::KEY_RELEASE],
            }],
        )?;

        Ok(Self {
            key_next,
            key_prev,
            key_kill,
            key_quit,
            key_mod,
            modifier,
        })
    }
}
struct IconCache {
    icons: HashMap<(String, String), Frame>,
}
impl IconCache {
    fn new() -> Self {
        Self {
            icons: HashMap::new(),
        }
    }
    fn refresh(&mut self, conn: &impl Connection, atoms: &AtomCollection, task: &Task) {
        if let Ok(icon) = get_net_wm_icon(conn, atoms, task.wid) {
            self.icons.insert(task.class.clone(), icon);
            return;
        }
        if let Ok(icon) = get_hicolor_icon(task) {
            self.icons.insert(task.class.clone(), icon);
            return;
        }
        if let Ok(Some(wid)) = get_window_parent(conn, atoms, task.wid)
            && let Some(parent) = window_to_task(conn, atoms, wid)
            && let Some(icon) = self.icons.get(&parent.class)
        {
            self.icons.insert(task.class.clone(), icon.clone());
            return;
        }
        self.icons.insert(task.class.clone(), Frame::new(0, 0));
    }
    fn cache(&mut self, conn: &impl Connection, atoms: &AtomCollection, tasks: &TaskList) {
        for task in tasks.list_ascending().0 {
            if !self.icons.contains_key(&task.class) {
                self.refresh(conn, atoms, task);
            }
        }
    }
    fn get(&mut self, task: &Task) -> &Frame {
        self.icons.get(&task.class).unwrap()
    }
}
fn create_window(
    conn: &impl Connection,
    screen: &Screen,
    atoms: &AtomCollection,
    geometry: Area,
    depth: u8,
    visual: Visualid,
) -> Result<Window> {
    let window = conn.generate_id()?;
    let colormap = conn.generate_id()?;
    conn.create_colormap(ColormapAlloc::NONE, colormap, screen.root, visual)?;
    let win_aux = CreateWindowAux::new()
        .event_mask(EventMask::EXPOSURE | EventMask::KEY_PRESS | EventMask::KEY_RELEASE)
        .colormap(colormap)
        .override_redirect(1);
    conn.create_window(
        depth,
        window,
        screen.root,
        geometry.x as i16,
        geometry.y as i16,
        geometry.w as u16,
        geometry.h as u16,
        0,
        WindowClass::INPUT_OUTPUT,
        visual,
        &win_aux,
    )?;
    conn.change_property8(
        PropMode::REPLACE,
        window,
        atoms.WM_NAME,
        atoms.STRING,
        APP_NAME.as_bytes(),
    )?;
    conn.change_property8(
        PropMode::REPLACE,
        window,
        atoms._NET_WM_NAME,
        atoms.UTF8_STRING,
        APP_NAME.as_bytes(),
    )?;
    conn.change_property8(
        PropMode::REPLACE,
        window,
        atoms.WM_CLASS,
        atoms.STRING,
        APP_NAME.as_bytes(),
    )?;
    conn.change_property32(
        PropMode::REPLACE,
        window,
        atoms._NET_WM_STATE,
        atoms.ATOM,
        &[atoms._NET_WM_STATE_SKIP_TASKBAR, atoms._NET_WM_STATE_ABOVE],
    )?;
    conn.change_property32(
        PropMode::REPLACE,
        window,
        atoms._NET_WM_WINDOW_TYPE,
        atoms.ATOM,
        &[atoms._NET_WM_WINDOW_TYPE_DIALOG],
    )?;

    Ok(window)
}
fn send_frame(
    conn: &impl Connection,
    wid: Window,
    gc: Gcontext,
    frame: &Frame,
    depth: u8,
) -> Result<()> {
    let format = ImageFormat::Z_PIXMAP;
    let w = frame.width() as u16;
    let h = frame.height() as u16;
    conn.put_image(format, wid, gc, w, h, 0, 0, 0, depth, frame.buf_u8())?;
    Ok(())
}
fn request_window_close(conn: &impl Connection, atoms: &AtomCollection, wid: Window) -> Result<()> {
    let ev = ClientMessageEvent {
        response_type: CLIENT_MESSAGE_EVENT,
        format: 32,
        sequence: 0,
        window: wid,
        type_: atoms.WM_PROTOCOLS,
        data: ClientMessageData::from([atoms.WM_DELETE_WINDOW, x11rb::CURRENT_TIME, 0, 0, 0]),
    };
    conn.send_event(false, wid, EventMask::NO_EVENT, ev)?;
    Ok(())
}
fn request_window_focus(
    conn: &impl Connection,
    screen: &Screen,
    atoms: &AtomCollection,
    wid: Window,
) -> Result<()> {
    conn.send_event(
        false,
        screen.root,
        EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
        ClientMessageEvent {
            response_type: CLIENT_MESSAGE_EVENT,
            format: 32,
            sequence: 0,
            window: wid,
            type_: atoms._NET_ACTIVE_WINDOW,
            data: ClientMessageData::from([1, x11rb::CURRENT_TIME, 0, 0, 0]),
        },
    )?;
    Ok(())
}
fn request_window_move(conn: &impl Connection, wid: Window, area: Area) -> Result<()> {
    conn.configure_window(
        wid,
        &ConfigureWindowAux::new()
            .x(area.x as i32)
            .y(area.y as i32)
            .width(area.w as u32)
            .height(area.h as u32),
    )?;
    Ok(())
}
fn create_graphic_context(conn: &impl Connection, window: Window) -> Result<u32> {
    let gc = conn.generate_id()?;
    conn.create_gc(gc, window, &CreateGCAux::new())?;
    Ok(gc)
}
fn choose_visual(conn: &impl Connection, screen_num: usize) -> Result<(u8, Visualid)> {
    let depth = 32;
    let screen = &conn.setup().roots[screen_num];
    let has_render = conn
        .extension_information(render::X11_EXTENSION_NAME)?
        .is_some();

    if has_render {
        let formats = conn.render_query_pict_formats()?.reply()?;
        let format = formats
            .formats
            .iter()
            .filter(|info| (info.type_, info.depth) == (PictType::DIRECT, depth))
            .filter(|info| {
                let d = info.direct;
                (d.red_mask, d.green_mask, d.blue_mask, d.alpha_mask) == (0xff, 0xff, 0xff, 0xff)
            })
            .find(|info| {
                let d = info.direct;
                (d.red_shift, d.green_shift, d.blue_shift, d.alpha_shift)
                    == (16, 8, 0, depth.into())
            });
        if let Some(format) = format
            && let Some(visual) = formats.screens[screen_num]
                .depths
                .iter()
                .flat_map(|d| &d.visuals)
                .find(|v| v.format == format.id)
        {
            return Ok((format.depth, visual.visual));
        }
    }
    Ok((screen.root_depth, screen.root_visual))
}
fn get_active_window(
    conn: &impl Connection,
    screen: &Screen,
    atoms: &AtomCollection,
) -> Result<Option<Window>> {
    let prop = conn
        .get_property(
            false,
            screen.root,
            atoms._NET_ACTIVE_WINDOW,
            atoms.WINDOW,
            0,
            u32::MAX,
        )?
        .reply()?;

    Ok(prop.value32().and_then(|mut val| match val.next() {
        None => None,
        Some(0) => None,
        Some(wid) => Some(wid),
    }))
}
fn get_windows(
    conn: &impl Connection,
    screen: &Screen,
    atoms: &AtomCollection,
) -> Result<Vec<Window>> {
    let net_client_list = conn.intern_atom(false, b"_NET_CLIENT_LIST")?.reply()?.atom;
    let prop = conn
        .get_property(
            false,
            screen.root,
            net_client_list,
            atoms.WINDOW,
            0,
            u32::MAX,
        )?
        .reply()?;
    let windows = prop
        .value32()
        .ok_or("failed to extract windows")?
        .collect::<Vec<_>>();
    Ok(windows)
}
fn get_window_title(conn: &impl Connection, atoms: &AtomCollection, wid: Window) -> Result<String> {
    let bytes: Result<Vec<u8>> = conn
        .get_property(
            false,
            wid,
            atoms._NET_WM_NAME,
            atoms.UTF8_STRING,
            0,
            u32::MAX,
        )
        .map_err(Into::into)
        .and_then(|prop| prop.reply().map(|v| v.value).map_err(Into::into));
    if let Ok(bytes) = bytes {
        return Ok(String::from_utf8(bytes)?);
    }
    let bytes = conn
        .get_property(false, wid, atoms.WM_NAME, atoms.UTF8_STRING, 0, u32::MAX)?
        .reply()?
        .value;
    Ok(String::from_utf8(bytes)?)
}
fn get_window_class(
    conn: &impl Connection,
    atoms: &AtomCollection,
    wid: Window,
) -> Result<(String, String)> {
    let bytes = conn
        .get_property(false, wid, atoms.WM_CLASS, atoms.STRING, 0, u32::MAX)?
        .reply()?
        .value;
    let mut parts = bytes.split(|b| *b == 0);
    let instance = parts
        .next()
        .and_then(|s| String::from_utf8(s.to_vec()).ok())
        .unwrap_or_default();
    let class = parts
        .next()
        .and_then(|s| String::from_utf8(s.to_vec()).ok())
        .unwrap_or_default();
    Ok((instance, class))
}
fn get_window_parent(
    conn: &impl Connection,
    atoms: &AtomCollection,
    wid: Window,
) -> Result<Option<Window>> {
    let reply = conn
        .get_property(false, wid, atoms.WM_TRANSIENT_FOR, atoms.WINDOW, 0, 1)?
        .reply()?;
    if reply.value_len == 0 {
        Ok(None)
    } else {
        let window_id = u32::from_ne_bytes(reply.value[..4].try_into()?);
        Ok(Some(window_id))
    }
}
fn _get_window_pid(
    conn: &impl Connection,
    atoms: &AtomCollection,
    wid: Window,
) -> Result<Option<u32>> {
    let reply = conn
        .get_property::<_, u32>(false, wid, atoms._NET_WM_PID, atoms.CARDINAL, 0, 1)?
        .reply()?;
    let mut pids = reply.value32().ok_or_else(|| "no pid".to_string())?;
    Ok(pids.next())
}
fn get_net_wm_icon(conn: &impl Connection, atoms: &AtomCollection, wid: Window) -> Result<Frame> {
    let reply = conn
        .get_property(false, wid, atoms._NET_WM_ICON, atoms.CARDINAL, 0, u32::MAX)?
        .reply()?;
    let Some(it) = reply.value32() else {
        return Err("no _NET_WM_ICON".into());
    };
    let bytes = it.collect::<Vec<_>>();
    let mut bytes = bytes.as_slice();
    let mut biggest: Option<(usize, usize, &[u32])> = None;

    loop {
        if bytes.len() < 2 {
            break;
        }
        let w = bytes[0] as usize;
        let h = bytes[1] as usize;
        let step = w * h;
        bytes = &bytes[2..];
        if bytes.len() < step {
            break;
        }
        let curr = (w, h, &bytes[0..step]);
        match biggest {
            Some((pw, ph, _)) => {
                if w * h > pw * ph {
                    biggest = Some(curr)
                }
            }
            None => biggest = Some(curr),
        }
        bytes = &bytes[step..];
    }
    if let Some((w, h, data)) = biggest {
        let icon = Frame::from_argb_u32(data, w as u32, h as u32);
        return Ok(icon);
    }
    Err("no _net_wm_icon".into())
}
fn get_hicolor_icon(task: &Task) -> Result<Frame> {
    let hicolor = PathBuf::from(HICOLOR);
    let search_term = task.class.1.to_lowercase();
    let mut biggest: Option<Frame> = None;
    let files = visit_dir(hicolor)?;
    for file in files {
        let Some(filename) = file.file_name().map(|f| f.to_string_lossy()) else {
            continue;
        };
        if filename.to_lowercase().contains(&search_term) {
            let ext = file.extension().and_then(|s| s.to_str());
            let img = if ext == Some("png") {
                //let Ok(pm) = Pixmap::load_png(file) else {
                //    continue;
                //};
                //pm
                continue;
            } else if ext == Some("svg") {
                let svg = nsvg::parse_file(&file, nsvg::Units::Pixel, 96.0).unwrap();
                let Ok(image) = svg.rasterize(1.0) else {
                    continue;
                };
                let (w, h) = (image.width(), image.height());
                Frame::from_rgba_u8(&image, w, h)
            } else {
                continue;
            };

            match &biggest {
                Some(icon) => {
                    if img.width() * img.height() > icon.width() * icon.height() {
                        biggest = Some(img);
                    }
                }
                None => {
                    biggest = Some(img);
                }
            }
        }
    }
    if let Some(icon) = biggest {
        return Ok(icon);
    }
    Err("no hicolor icon".into())
}
fn get_dpi(db: &Database, screen: &Screen) -> Result<f32> {
    if let Ok(Some(dpi)) = db.get_value("Xft.dpi", "") {
        return Ok(dpi);
    }
    let dpi_x = screen.width_in_pixels as f32 * INCH_TO_MM / screen.width_in_millimeters as f32;
    let dpi_y = screen.height_in_pixels as f32 * INCH_TO_MM / screen.height_in_millimeters as f32;
    let dpi = (dpi_x + dpi_y) / 2.0;
    Ok(dpi)
}
fn window_to_task(conn: &impl Connection, atoms: &AtomCollection, wid: Window) -> Option<Task> {
    let attr = conn.get_window_attributes(wid).ok()?.reply().ok()?;
    if attr.override_redirect {
        return None;
    }
    let title = get_window_title(conn, atoms, wid).ok()?;
    let class = get_window_class(conn, atoms, wid).ok()?;
    // let pid = get_window_pid(conn, atoms, wid).ok()?;
    Some(Task { wid, title, class })
}
fn apply_dpi(val: f32, dpi: f32) -> f32 {
    val * dpi / 72.0
}
fn compute_window_geometry(conf: &Config, screen: &Screen, tasks: usize) -> Option<Area> {
    match conf.layout {
        ListLayout::Rows => compute_window_geometry_row(conf, screen, tasks),
        ListLayout::Columns => compute_window_geometry_col(conf, screen, tasks),
    }
}
fn compute_window_geometry_row(conf: &Config, screen: &Screen, tasks: usize) -> Option<Area> {
    if tasks == 0 {
        return None;
    }
    let screen_size = screen.height_in_pixels as f32;
    let task_h = compute_task_size(conf, screen_size, conf.task_height, tasks);
    let w = conf.width;
    let h = task_h * tasks as f32;
    let screen_w = screen.width_in_pixels as f32;
    let screen_h = screen.height_in_pixels as f32;
    let (x, y) = conf.location.resolve((w, h), (screen_w, screen_h));
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    Some((x, y, w, h).into())
}
fn compute_window_geometry_col(conf: &Config, screen: &Screen, tasks: usize) -> Option<Area> {
    if tasks == 0 {
        return None;
    }
    let screen_size = screen.width_in_pixels as f32;
    let task_size = compute_task_size(conf, screen_size, conf.task_width, tasks);
    let w = task_size * tasks as f32;
    let h = conf.height;
    let screen_w = screen.width_in_pixels as f32;
    let screen_h = screen.height_in_pixels as f32;
    let (x, y) = conf.location.resolve((w, h), (screen_w, screen_h));
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    Some((x, y, w, h).into())
}
fn compute_task_size(conf: &Config, screen_size: f32, task_size: Size, tasks: usize) -> f32 {
    let bw = conf.border_width * 2.0;
    let screen_size = screen_size - bw;
    let task_size = task_size.resolve(screen_size);
    let content_h = task_size * tasks as f32 + bw;
    if content_h <= screen_size {
        task_size
    } else {
        (screen_size - bw) / tasks as f32
    }
}
fn visit_dir(dir: PathBuf) -> Result<Vec<PathBuf>> {
    let mut files = vec![];
    let mut dirs = vec![dir];

    while let Some(dir) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.filter_map(|entry| entry.ok()) {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
            } else {
                files.push(path);
            }
        }
    }
    Ok(files)
}
