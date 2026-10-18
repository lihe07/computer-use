//! List and control toplevel windows via ext-foreign-toplevel-list + COSMIC toplevel protocols.

use std::collections::HashMap;

use anyhow::{Context, Result, anyhow, bail};
use cosmic_protocols::toplevel_info::v1::client::{zcosmic_toplevel_handle_v1, zcosmic_toplevel_info_v1};
use cosmic_protocols::toplevel_management::v1::client::zcosmic_toplevel_manager_v1;
use serde::Serialize;
use wayland_client::backend::ObjectId;
use wayland_client::protocol::{wl_output, wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, event_created_child};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1 as ext_handle, ext_foreign_toplevel_list_v1 as ext_list,
};

#[derive(Clone, Debug, Default, Serialize)]
pub struct Geometry {
    pub output: String,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Window {
    pub id: String,
    pub title: String,
    pub app_id: String,
    pub states: Vec<&'static str>,
    pub geometry: Vec<Geometry>,
    #[serde(skip)]
    closed: bool,
}

struct Top {
    ext: ext_handle::ExtForeignToplevelHandleV1,
    cosmic: Option<zcosmic_toplevel_handle_v1::ZcosmicToplevelHandleV1>,
    win: Window,
}

#[derive(Default)]
struct State {
    list: Option<ext_list::ExtForeignToplevelListV1>,
    info: Option<zcosmic_toplevel_info_v1::ZcosmicToplevelInfoV1>,
    mgr: Option<zcosmic_toplevel_manager_v1::ZcosmicToplevelManagerV1>,
    seat: Option<wl_seat::WlSeat>,
    outputs: HashMap<ObjectId, String>,
    tops: Vec<Top>,
    info_done: bool,
}

impl State {
    fn top_by_ext(&mut self, id: &ObjectId) -> Option<&mut Top> {
        self.tops.iter_mut().find(|t| &t.ext.id() == id)
    }
}

pub struct Windows {
    conn: Connection,
    queue: wayland_client::EventQueue<State>,
    state: State,
}

impl Windows {
    pub fn connect() -> Result<Self> {
        let conn = Connection::connect_to_env().context("connect to Wayland display")?;
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        conn.display().get_registry(&qh, ());
        let mut state = State::default();
        queue.roundtrip(&mut state)?; // globals bound
        if state.list.is_none() || state.info.is_none() {
            bail!("compositor lacks ext_foreign_toplevel_list_v1 / zcosmic_toplevel_info_v1");
        }
        queue.roundtrip(&mut state)?; // toplevels announced
        let info = state.info.clone().unwrap();
        for (i, t) in state.tops.iter_mut().enumerate() {
            t.cosmic = Some(info.get_cosmic_toplevel(&t.ext, &qh, i));
        }
        // cosmic-comp sends handle state on its next refresh, ending with info.done.
        state.info_done = false;
        let end = std::time::Instant::now() + std::time::Duration::from_millis(500);
        while !state.info_done && std::time::Instant::now() < end {
            queue.roundtrip(&mut state)?;
            if !state.info_done {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        Ok(Self { conn, queue, state })
    }

    pub fn list(&self) -> Vec<Window> {
        self.state.tops.iter().filter(|t| !t.win.closed).map(|t| t.win.clone()).collect()
    }

    fn find(&self, query: &str) -> Result<&Top> {
        let tops: Vec<&Top> = self.state.tops.iter().filter(|t| !t.win.closed).collect();
        if let Some(t) = tops.iter().find(|t| t.win.id == query) {
            return Ok(t);
        }
        let q = query.to_lowercase();
        let hits: Vec<&&Top> = tops
            .iter()
            .filter(|t| t.win.title.to_lowercase().contains(&q) || t.win.app_id.to_lowercase().contains(&q))
            .collect();
        match hits.len() {
            1 => Ok(hits[0]),
            0 => bail!("no window matches {query:?}"),
            n => bail!(
                "{n} windows match {query:?}; use an id: {}",
                hits.iter().map(|t| format!("{} ({})", t.win.id, t.win.title)).collect::<Vec<_>>().join(", ")
            ),
        }
    }

    /// Apply an action to a window matched by id or a unique title/app_id substring.
    pub fn act(&mut self, query: &str, action: &str) -> Result<Window> {
        let mgr = self.state.mgr.clone().ok_or_else(|| anyhow!("no zcosmic_toplevel_manager_v1"))?;
        let top = self.find(query)?;
        let h = top.cosmic.clone().ok_or_else(|| anyhow!("window has no cosmic handle"))?;
        let win = top.win.clone();
        match action {
            "activate" | "focus" => {
                let seat = self.state.seat.clone().ok_or_else(|| anyhow!("no wl_seat"))?;
                mgr.activate(&h, &seat);
            }
            "close" => mgr.close(&h),
            "maximize" => mgr.set_maximized(&h),
            "unmaximize" => mgr.unset_maximized(&h),
            "minimize" => mgr.set_minimized(&h),
            "unminimize" => mgr.unset_minimized(&h),
            "fullscreen" => mgr.set_fullscreen(&h, None),
            "unfullscreen" => mgr.unset_fullscreen(&h),
            _ => bail!("unknown window action {action:?}"),
        }
        self.conn.flush()?;
        self.queue.roundtrip(&mut self.state)?;
        Ok(win)
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(s: &mut Self, reg: &wl_registry::WlRegistry, ev: wl_registry::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let wl_registry::Event::Global { name, interface, version } = ev {
            match interface.as_str() {
                "ext_foreign_toplevel_list_v1" => s.list = Some(reg.bind(name, 1, qh, ())),
                "zcosmic_toplevel_info_v1" if version >= 2 => s.info = Some(reg.bind(name, version.min(3), qh, ())),
                "zcosmic_toplevel_manager_v1" => s.mgr = Some(reg.bind(name, version.min(4), qh, ())),
                "wl_seat" if s.seat.is_none() => s.seat = Some(reg.bind(name, 1, qh, ())),
                "wl_output" if version >= 4 => {
                    let _: wl_output::WlOutput = reg.bind(name, 4, qh, ());
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(s: &mut Self, o: &wl_output::WlOutput, ev: wl_output::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_output::Event::Name { name } = ev {
            s.outputs.insert(o.id(), name);
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(_: &mut Self, _: &wl_seat::WlSeat, _: wl_seat::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ext_list::ExtForeignToplevelListV1, ()> for State {
    fn event(s: &mut Self, _: &ext_list::ExtForeignToplevelListV1, ev: ext_list::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let ext_list::Event::Toplevel { toplevel } = ev {
            s.tops.push(Top { ext: toplevel, cosmic: None, win: Window::default() });
        }
    }

    event_created_child!(State, ext_list::ExtForeignToplevelListV1, [
        ext_list::EVT_TOPLEVEL_OPCODE => (ext_handle::ExtForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ext_handle::ExtForeignToplevelHandleV1, ()> for State {
    fn event(s: &mut Self, h: &ext_handle::ExtForeignToplevelHandleV1, ev: ext_handle::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        let Some(t) = s.top_by_ext(&h.id()) else { return };
        match ev {
            ext_handle::Event::Title { title } => t.win.title = title,
            ext_handle::Event::AppId { app_id } => t.win.app_id = app_id,
            ext_handle::Event::Identifier { identifier } => t.win.id = identifier,
            ext_handle::Event::Closed => t.win.closed = true,
            _ => {}
        }
    }
}

impl Dispatch<zcosmic_toplevel_info_v1::ZcosmicToplevelInfoV1, ()> for State {
    fn event(s: &mut Self, _: &zcosmic_toplevel_info_v1::ZcosmicToplevelInfoV1, ev: zcosmic_toplevel_info_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let zcosmic_toplevel_info_v1::Event::Done = ev {
            s.info_done = true;
        }
    }
}

impl Dispatch<zcosmic_toplevel_handle_v1::ZcosmicToplevelHandleV1, usize> for State {
    fn event(s: &mut Self, _: &zcosmic_toplevel_handle_v1::ZcosmicToplevelHandleV1, ev: zcosmic_toplevel_handle_v1::Event, idx: &usize, _: &Connection, _: &QueueHandle<Self>) {
        use zcosmic_toplevel_handle_v1::{Event, State as St};
        let outputs = &s.outputs;
        let Some(t) = s.tops.get_mut(*idx) else { return };
        match ev {
            Event::State { state } => {
                t.win.states = state
                    .chunks_exact(4)
                    .filter_map(|c| match St::try_from(u32::from_ne_bytes([c[0], c[1], c[2], c[3]])) {
                        Ok(St::Maximized) => Some("maximized"),
                        Ok(St::Minimized) => Some("minimized"),
                        Ok(St::Activated) => Some("focused"),
                        Ok(St::Fullscreen) => Some("fullscreen"),
                        Ok(St::Sticky) => Some("sticky"),
                        _ => None,
                    })
                    .collect();
            }
            Event::Geometry { output, x, y, width, height } => {
                let name = outputs.get(&output.id()).cloned().unwrap_or_default();
                t.win.geometry.retain(|g| g.output != name);
                t.win.geometry.push(Geometry { output: name, x, y, width, height });
            }
            Event::OutputLeave { output } => {
                let name = outputs.get(&output.id()).cloned().unwrap_or_default();
                t.win.geometry.retain(|g| g.output != name);
            }
            _ => {}
        }
    }
}

impl Dispatch<zcosmic_toplevel_manager_v1::ZcosmicToplevelManagerV1, ()> for State {
    fn event(_: &mut Self, _: &zcosmic_toplevel_manager_v1::ZcosmicToplevelManagerV1, _: zcosmic_toplevel_manager_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

