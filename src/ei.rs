//! Minimal sender-side libei client over FFI.

use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::os::fd::{IntoRawFd, OwnedFd};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};

pub const CAP_POINTER: u32 = 1 << 0;
pub const CAP_POINTER_ABSOLUTE: u32 = 1 << 1;
pub const CAP_KEYBOARD: u32 = 1 << 2;
pub const CAP_SCROLL: u32 = 1 << 4;
pub const CAP_BUTTON: u32 = 1 << 5;
const ALL_CAPS: [u32; 5] = [CAP_POINTER, CAP_POINTER_ABSOLUTE, CAP_KEYBOARD, CAP_SCROLL, CAP_BUTTON];

const EV_CONNECT: c_int = 1;
const EV_DISCONNECT: c_int = 2;
const EV_SEAT_ADDED: c_int = 3;
const EV_DEVICE_ADDED: c_int = 5;
const EV_DEVICE_REMOVED: c_int = 6;
const EV_DEVICE_PAUSED: c_int = 7;
const EV_DEVICE_RESUMED: c_int = 8;

type P = *mut c_void;

#[link(name = "ei")]
unsafe extern "C" {
    fn ei_new_sender(user_data: P) -> P;
    fn ei_unref(ei: P) -> P;
    fn ei_configure_name(ei: P, name: *const c_char);
    fn ei_setup_backend_fd(ei: P, fd: c_int) -> c_int;
    fn ei_get_fd(ei: P) -> c_int;
    fn ei_dispatch(ei: P);
    fn ei_get_event(ei: P) -> P;
    fn ei_now(ei: P) -> u64;
    fn ei_event_unref(ev: P) -> P;
    fn ei_event_get_type(ev: P) -> c_int;
    fn ei_event_get_seat(ev: P) -> P;
    fn ei_event_get_device(ev: P) -> P;
    fn ei_seat_has_capability(seat: P, cap: u32) -> bool;
    fn ei_seat_bind_capabilities(seat: P, ...);
    fn ei_device_ref(dev: P) -> P;
    fn ei_device_unref(dev: P) -> P;
    fn ei_device_has_capability(dev: P, cap: u32) -> bool;
    fn ei_device_get_region(dev: P, index: usize) -> P;
    fn ei_region_get_x(r: P) -> u32;
    fn ei_region_get_y(r: P) -> u32;
    fn ei_region_get_width(r: P) -> u32;
    fn ei_region_get_height(r: P) -> u32;
    fn ei_region_get_physical_scale(r: P) -> f64;
    fn ei_region_get_mapping_id(r: P) -> *const c_char;
    fn ei_device_start_emulating(dev: P, sequence: u32);
    fn ei_device_frame(dev: P, time: u64);
    fn ei_device_pointer_motion_absolute(dev: P, x: f64, y: f64);
    fn ei_device_button_button(dev: P, button: u32, is_press: bool);
    fn ei_device_scroll_discrete(dev: P, x: i32, y: i32);
    fn ei_device_keyboard_key(dev: P, keycode: u32, is_press: bool);
}

#[derive(Clone, Debug)]
pub struct Region {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    pub scale: f64,
    pub name: String,
}

struct Device {
    ptr: P,
    caps: u32,
    regions: Vec<Region>,
    resumed: bool,
    emulating: bool,
}

pub struct EiClient {
    ei: P,
    devices: HashMap<usize, Device>,
    pub disconnected: bool,
    seq: u32,
}

// The client is only ever used behind a Mutex from one thread at a time.
unsafe impl Send for EiClient {}

fn cstr(p: *const c_char) -> String {
    if p.is_null() { String::new() } else { unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned() }
}

impl EiClient {
    pub fn new(fd: OwnedFd) -> Result<Self> {
        unsafe {
            let ei = ei_new_sender(std::ptr::null_mut());
            let name = CString::new("claude-computer-use").unwrap();
            ei_configure_name(ei, name.as_ptr());
            let rc = ei_setup_backend_fd(ei, fd.into_raw_fd());
            if rc != 0 {
                ei_unref(ei);
                bail!("ei_setup_backend_fd failed: {rc}");
            }
            Ok(Self { ei, devices: HashMap::new(), disconnected: false, seq: 1 })
        }
    }

    /// Process pending compositor events, waiting up to `timeout` for new ones.
    pub fn pump(&mut self, timeout: Duration) {
        unsafe {
            let mut pfd = libc_pollfd { fd: ei_get_fd(self.ei), events: 1, revents: 0 };
            if poll(&mut pfd, 1, timeout.as_millis() as c_int) > 0 {
                ei_dispatch(self.ei);
            }
            loop {
                let ev = ei_get_event(self.ei);
                if ev.is_null() {
                    break;
                }
                match ei_event_get_type(ev) {
                    EV_CONNECT => {}
                    EV_DISCONNECT => self.disconnected = true,
                    EV_SEAT_ADDED => {
                        let seat = ei_event_get_seat(ev);
                        let caps: Vec<u32> =
                            ALL_CAPS.iter().copied().filter(|&c| ei_seat_has_capability(seat, c)).collect();
                        // Variadic, NULL-terminated; we never have more than 5 caps.
                        let mut a = [0 as P; 6];
                        for (i, c) in caps.iter().enumerate() {
                            a[i] = *c as usize as P;
                        }
                        ei_seat_bind_capabilities(seat, a[0], a[1], a[2], a[3], a[4], a[5]);
                    }
                    EV_DEVICE_ADDED => {
                        let d = ei_event_get_device(ev);
                        let caps = ALL_CAPS.iter().filter(|&&c| ei_device_has_capability(d, c)).fold(0, |a, c| a | c);
                        let mut regions = Vec::new();
                        let mut i = 0;
                        loop {
                            let r = ei_device_get_region(d, i);
                            if r.is_null() {
                                break;
                            }
                            regions.push(Region {
                                x: ei_region_get_x(r),
                                y: ei_region_get_y(r),
                                w: ei_region_get_width(r),
                                h: ei_region_get_height(r),
                                scale: ei_region_get_physical_scale(r),
                                name: cstr(ei_region_get_mapping_id(r)),
                            });
                            i += 1;
                        }
                        self.devices.insert(d as usize, Device {
                            ptr: ei_device_ref(d),
                            caps,
                            regions,
                            resumed: false,
                            emulating: false,
                        });
                    }
                    EV_DEVICE_REMOVED => {
                        if let Some(dev) = self.devices.remove(&(ei_event_get_device(ev) as usize)) {
                            ei_device_unref(dev.ptr);
                        }
                    }
                    EV_DEVICE_RESUMED => {
                        if let Some(dev) = self.devices.get_mut(&(ei_event_get_device(ev) as usize)) {
                            dev.resumed = true;
                        }
                    }
                    EV_DEVICE_PAUSED => {
                        if let Some(dev) = self.devices.get_mut(&(ei_event_get_device(ev) as usize)) {
                            dev.resumed = false;
                            dev.emulating = false;
                        }
                    }
                    _ => {}
                }
                ei_event_unref(ev);
            }
        }
    }

    /// Pump until resumed devices cover `caps`.
    pub fn wait_ready(&mut self, caps: u32, timeout: Duration) -> Result<()> {
        let end = Instant::now() + timeout;
        while Instant::now() < end {
            self.pump(Duration::from_millis(50));
            if self.disconnected {
                bail!("EIS disconnected");
            }
            let have = self.devices.values().filter(|d| d.resumed).fold(0, |a, d| a | d.caps);
            if have & caps == caps {
                return Ok(());
            }
        }
        bail!("EIS devices not ready")
    }

    fn device(&mut self, cap: u32) -> Result<&mut Device> {
        self.devices
            .values_mut()
            .find(|d| d.resumed && d.caps & cap != 0)
            .ok_or_else(|| anyhow!("no resumed EI device with capability {cap:#x}"))
    }

    pub fn regions(&mut self) -> Result<Vec<Region>> {
        Ok(self.device(CAP_POINTER_ABSOLUTE)?.regions.clone())
    }

    fn emit(&mut self, cap: u32, f: impl FnOnce(P)) -> Result<()> {
        let ei = self.ei;
        let seq = self.seq;
        let dev = self.device(cap)?;
        let ptr = dev.ptr;
        let started = !dev.emulating;
        dev.emulating = true;
        unsafe {
            if started {
                ei_device_start_emulating(ptr, seq);
            }
            f(ptr);
            ei_device_frame(ptr, ei_now(ei));
        }
        if started {
            self.seq += 1;
        }
        self.pump(Duration::ZERO);
        Ok(())
    }

    pub fn move_abs(&mut self, x: f64, y: f64) -> Result<()> {
        self.emit(CAP_POINTER_ABSOLUTE, |d| unsafe { ei_device_pointer_motion_absolute(d, x, y) })
    }

    pub fn button(&mut self, code: u32, press: bool) -> Result<()> {
        self.emit(CAP_BUTTON, |d| unsafe { ei_device_button_button(d, code, press) })
    }

    /// dx/dy in wheel notches (positive = right/down).
    pub fn scroll(&mut self, dx: f64, dy: f64) -> Result<()> {
        let (x, y) = ((dx * 120.0) as i32, (dy * 120.0) as i32);
        self.emit(CAP_SCROLL, |d| unsafe { ei_device_scroll_discrete(d, x, y) })
    }

    /// evdev keycode (KEY_*), not offset by 8.
    pub fn key(&mut self, code: u32, press: bool) -> Result<()> {
        self.emit(CAP_KEYBOARD, |d| unsafe { ei_device_keyboard_key(d, code, press) })
    }
}

impl Drop for EiClient {
    fn drop(&mut self) {
        unsafe {
            for d in self.devices.values() {
                ei_device_unref(d.ptr);
            }
            ei_unref(self.ei);
        }
    }
}

#[repr(C)]
struct libc_pollfd {
    fd: c_int,
    events: i16,
    revents: i16,
}

unsafe extern "C" {
    fn poll(fds: *mut libc_pollfd, nfds: u64, timeout: c_int) -> c_int;
}
