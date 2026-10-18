//! Nonexclusive background track shortcuts: XInput for Xbox, Raw Input for Sony DualSense.
//! Raw Input uses a private message-only window, so closing the main window to the tray is safe.
//! https://learn.microsoft.com/windows/win32/inputdev/about-raw-input

use std::cell::RefCell;
use std::collections::HashMap;
use std::mem::{offset_of, size_of};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use gpui::App;
use state::Sonora;
use tokio::sync::mpsc::{self, UnboundedSender};
use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::XboxController::{
    XINPUT_GAMEPAD_BACK, XINPUT_GAMEPAD_LEFT_SHOULDER, XINPUT_GAMEPAD_RIGHT_SHOULDER,
    XINPUT_STATE, XInputGetState,
};
use windows::Win32::UI::Input::{
    GetRawInputData, GetRawInputDeviceInfoW, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE,
    RAWINPUTHEADER, RAWHID, RID_DEVICE_INFO, RIDEV_DEVNOTIFY, RIDEV_INPUTSINK,
    RIDI_DEVICEINFO, RID_INPUT, RIM_TYPEHID, RegisterRawInputDevices,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
    HWND_MESSAGE, MSG, PostQuitMessage, RegisterClassW, SetTimer, WINDOW_EX_STYLE,
    WINDOW_STYLE, WM_CLOSE, WM_DESTROY, WM_INPUT, WM_INPUT_DEVICE_CHANGE, WM_TIMER,
    WNDCLASSW,
};
use windows::core::w;

const POLL_MS: u32 = 40;
const RECONNECT: Duration = Duration::from_secs(1);
/// Suppress the second copy when a mapper exposes one DualSense through both native and XInput.
const DEDUP: Duration = Duration::from_millis(250);
const STALE: Duration = Duration::from_secs(1);
const MODIFIER: u8 = 1;
const PREVIOUS: u8 = 2;
const NEXT: u8 = 4;
const SONY: u32 = 0x054c;
const DUALSENSE: [u32; 2] = [0x0ce6, 0x0df2];
const MAX_PACKET: u32 = 64 * 1024;

/// The only commands the reader can send; it never opens windows or changes focus.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Command {
    Previous,
    Next,
}

struct Event {
    command: Command,
    at: Instant,
}

/// The first sample is a baseline. Only a new bumper press with the modifier held acts.
#[derive(Default)]
struct Edge {
    previous: Option<u8>,
}

impl Edge {
    fn read(&mut self, buttons: u8, enabled: bool) -> Option<Command> {
        let previous = self.previous.replace(buttons)?;
        if !enabled || buttons & MODIFIER == 0 {
            return None;
        }
        let pressed = buttons & !previous & (PREVIOUS | NEXT);
        match pressed {
            PREVIOUS if buttons & NEXT == 0 => Some(Command::Previous),
            NEXT if buttons & PREVIOUS == 0 => Some(Command::Next),
            _ => None,
        }
    }
}

/// Native handles and caches remain on their owning thread, never inside the GPUI app.
struct Reader {
    enabled: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
    sender: UnboundedSender<Event>,
    xbox: [Edge; 4],
    next_probe: [Instant; 4],
    sony: HashMap<isize, Option<Edge>>,
    buffer: Vec<u64>,
    last: Option<(Command, Instant)>,
}

thread_local! {
    static READER: RefCell<Option<Reader>> = const { RefCell::new(None) };
}

/// Keeps the preference live and routes controller events through the existing Playback entity.
pub fn install(cx: &mut App) {
    let settings = Sonora::global(cx).settings.clone();
    let enabled = Arc::new(AtomicBool::new(settings.read(cx).controller_shortcuts()));
    let observed = enabled.clone();
    cx.observe(&settings, move |settings, cx| {
        observed.store(settings.read(cx).controller_shortcuts(), Ordering::Relaxed);
    }).detach();
    let stopped = Arc::new(AtomicBool::new(false));
    let stop = stopped.clone();
    cx.on_app_quit(move |_| {
        stop.store(true, Ordering::Relaxed);
        async {}
    }).detach();
    let (sender, mut receiver) = mpsc::unbounded_channel();
    let started = std::thread::Builder::new().name("sonora-controller".into()).spawn(move || {
        if let Err(error) = run(Reader {
            enabled, stopped, sender, xbox: std::array::from_fn(|_| Edge::default()),
            next_probe: [Instant::now(); 4],
            sony: HashMap::new(), buffer: Vec::new(), last: None,
        }) {
            log::warn!("controller: cannot start background shortcuts: {error}");
        }
    });
    if let Err(error) = started {
        log::warn!("controller: cannot start input thread: {error}");
        return;
    }
    cx.spawn(async move |cx| {
        while let Some(event) = receiver.recv().await {
            cx.update(|cx| {
                if event.at.elapsed() > STALE || !Sonora::global(cx).settings.read(cx).controller_shortcuts() {
                    return;
                }
                let playback = Sonora::global(cx).playback.clone();
                playback.update(cx, |playback, cx| match event.command {
                    Command::Previous => playback.previous(cx),
                    Command::Next => playback.next(cx),
                });
            });
        }
    }).detach();
}

impl Reader {
    /// Deduplicates native and emulated copies of a press before crossing to the app thread.
    fn send(&mut self, command: Command) {
        let now = Instant::now();
        if self.last.is_some_and(|(last, at)| last == command && now.duration_since(at) < DEDUP) {
            return;
        }
        self.last = Some((command, now));
        self.sender.send(Event { command, at: now }).ok();
    }

    fn poll_xbox(&mut self) {
        let enabled = self.enabled.load(Ordering::Relaxed);
        let now = Instant::now();
        for index in 0..self.xbox.len() {
            if self.xbox[index].previous.is_none() && now < self.next_probe[index] { continue; }
            let mut state = XINPUT_STATE::default();
            if unsafe { XInputGetState(index as u32, &mut state) } != 0 {
                self.xbox[index] = Edge::default();
                self.next_probe[index] = now + RECONNECT;
                continue;
            }
            let bits = state.Gamepad.wButtons;
            let buttons = u8::from(bits.contains(XINPUT_GAMEPAD_BACK))
                | (u8::from(bits.contains(XINPUT_GAMEPAD_LEFT_SHOULDER)) * PREVIOUS)
                | (u8::from(bits.contains(XINPUT_GAMEPAD_RIGHT_SHOULDER)) * NEXT);
            if let Some(command) = self.xbox[index].read(buttons, enabled) {
                self.send(command);
            }
        }
    }

    /// Reads a bounded, aligned native packet and checks its flexible-array length before slicing.
    fn raw_input(&mut self, input: HRAWINPUT) {
        let mut needed = 0;
        let header_size = size_of::<RAWINPUTHEADER>() as u32;
        let hid_offset = offset_of!(RAWINPUT, data);
        let minimum = (hid_offset + size_of::<RAWHID>()) as u32;
        unsafe { GetRawInputData(input, RID_INPUT, None, &mut needed, header_size); }
        if !(minimum..=MAX_PACKET).contains(&needed) { return; }
        self.buffer.resize((needed as usize).div_ceil(size_of::<u64>()), 0);
        let copied = unsafe { GetRawInputData(input, RID_INPUT, Some(self.buffer.as_mut_ptr().cast()), &mut needed, header_size) };
        if !(minimum..=needed).contains(&copied) { return; }
        // u64 storage satisfies the header's alignment. Small Bluetooth reports need not fill
        // sizeof(RAWINPUT), whose union also reserves room for larger mouse/keyboard packets.
        let header = unsafe { &*self.buffer.as_ptr().cast::<RAWINPUTHEADER>() };
        if header.dwType != RIM_TYPEHID.0 { return; }
        let device = header.hDevice;
        let edge = self.sony.entry(device.0 as isize).or_insert_with(|| match is_dualsense(device) {
            true => Some(Edge::default()),
            false => None,
        });
        let Some(edge) = edge else { return; };
        let hid = unsafe { self.buffer.as_ptr().cast::<u8>().add(hid_offset).cast::<RAWHID>().read_unaligned() };
        let offset = hid_offset + offset_of!(RAWHID, bRawData);
        let stride = hid.dwSizeHid as usize;
        let Some(length) = stride.checked_mul(hid.dwCount as usize) else { return; };
        if stride == 0 || length > (copied as usize).saturating_sub(offset) { return; }
        // The validated flexible array is owned by buffer and kept alive throughout this loop.
        let reports = unsafe { std::slice::from_raw_parts(self.buffer.as_ptr().cast::<u8>().add(offset), length) };
        let enabled = self.enabled.load(Ordering::Relaxed);
        let mut commands = Vec::new();
        for report in reports.chunks_exact(stride) {
            if let Some(buttons) = dualsense_buttons(report)
                && let Some(command) = edge.read(buttons, enabled)
            {
                commands.push(command);
            }
        }
        for command in commands { self.send(command); }
    }
}

/// Restricts native report decoding to Sony's standard DualSense and DualSense Edge hardware ids.
fn is_dualsense(device: HANDLE) -> bool {
    let mut info = RID_DEVICE_INFO { cbSize: size_of::<RID_DEVICE_INFO>() as u32, ..Default::default() };
    let mut length = info.cbSize;
    let result = unsafe { GetRawInputDeviceInfoW(Some(device), RIDI_DEVICEINFO, Some((&mut info as *mut RID_DEVICE_INFO).cast()), &mut length) };
    if result == u32::MAX || info.dwType != RIM_TYPEHID { return false; }
    let hid = unsafe { info.Anonymous.hid };
    hid.dwVendorId == SONY && DUALSENSE.contains(&hid.dwProductId)
}

/// Sony's USB (01), Bluetooth basic (01), and Bluetooth enhanced (31) button layouts.
/// Protocol field positions are documented by SDL's HIDAPI PS5 driver; no output reports are sent.
/// https://github.com/libsdl-org/SDL/blob/main/src/joystick/hidapi/SDL_hidapi_ps5.c
fn dualsense_buttons(report: &[u8]) -> Option<u8> {
    let offset = match (*report.first()?, report.len()) {
        (0x01, 64) => 9,
        (0x01, 10..=78) => 6,
        (0x31, 78) => 10,
        _ => return None,
    };
    let bits = *report.get(offset)?;
    Some(u8::from(bits & 0x10 != 0)
        | (u8::from(bits & 0x01 != 0) * PREVIOUS)
        | (u8::from(bits & 0x02 != 0) * NEXT))
}

/// A private message loop keeps background reads independent of GPUI's windows and borrow rules.
fn run(reader: Reader) -> windows::core::Result<()> {
    unsafe {
        let module = GetModuleHandleW(None)?;
        let class = w!("SonoraControllerInput");
        let wc = WNDCLASSW { hInstance: module.into(), lpszClassName: class, lpfnWndProc: Some(window_proc), ..Default::default() };
        if RegisterClassW(&wc) == 0 { return Err(windows::core::Error::from_thread()); }
        let window = CreateWindowExW(WINDOW_EX_STYLE::default(), class, w!(""), WINDOW_STYLE::default(),
            0, 0, 0, 0, Some(HWND_MESSAGE), None, Some(module.into()), None)?;
        let devices = [RAWINPUTDEVICE { usUsagePage: 1, usUsage: 5,
            dwFlags: RIDEV_INPUTSINK | RIDEV_DEVNOTIFY, hwndTarget: window }];
        // RIDEV_INPUTSINK reads background packets without suppressing foreground game input.
        if let Err(error) = RegisterRawInputDevices(&devices, size_of::<RAWINPUTDEVICE>() as u32) {
            log::warn!("controller: native DualSense input unavailable: {error}");
        }
        READER.with(|slot| *slot.borrow_mut() = Some(reader));
        if SetTimer(Some(window), 1, POLL_MS, None) == 0 {
            let error = windows::core::Error::from_thread();
            DestroyWindow(window).ok();
            READER.with(|slot| slot.borrow_mut().take());
            return Err(error);
        }
        let mut message = MSG::default();
        while GetMessageW(&mut message, None, 0, 0).0 > 0 {
            DispatchMessageW(&message);
        }
        READER.with(|slot| slot.borrow_mut().take());
    }
    Ok(())
}

/// Native dispatch stays on the input thread and sends only small track commands to GPUI.
unsafe extern "system" fn window_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match message {
        WM_TIMER => {
            let stop = READER.with(|slot| {
                let mut slot = slot.borrow_mut();
                let Some(reader) = slot.as_mut() else { return false; };
                if reader.stopped.load(Ordering::Relaxed) || reader.sender.is_closed() { return true; }
                reader.poll_xbox();
                false
            });
            if stop { unsafe { DestroyWindow(hwnd).ok(); } }
            return LRESULT(0);
        }
        WM_INPUT => READER.with(|slot| {
            if let Some(reader) = slot.borrow_mut().as_mut() {
                reader.raw_input(HRAWINPUT(lparam.0 as *mut _));
            }
        }),
        WM_INPUT_DEVICE_CHANGE => READER.with(|slot| {
            if let Some(reader) = slot.borrow_mut().as_mut() {
                reader.sony.remove(&lparam.0);
            }
        }),
        WM_CLOSE => { unsafe { DestroyWindow(hwnd).ok(); } return LRESULT(0); }
        WM_DESTROY => { unsafe { PostQuitMessage(0); } return LRESULT(0); }
        _ => {}
    }
    // Required for foreground WM_INPUT cleanup as well as the ordinary window lifecycle.
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}
