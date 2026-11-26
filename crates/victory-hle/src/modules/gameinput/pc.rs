//! Optional desktop controls on the keyboard's existing stand-in pad.
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::Instant;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::VK_F6;
use windows_sys::Win32::UI::Input::{
    GetRawInputData, MOUSE_MOVE_ABSOLUTE, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER, RID_INPUT,
    RIDEV_CAPTUREMOUSE, RIDEV_NOLEGACY, RIDEV_REMOVE, RIM_TYPEMOUSE, RegisterRawInputDevices,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{self as wm, GetForegroundWindow};

use super::{A, KeyState, LEFT_STICK, PadState, X, Y, fallback, merged};

static CAPTURED: AtomicUsize = AtomicUsize::new(0);

pub(super) fn enabled() -> bool {
    static ENABLED: LazyLock<bool> =
        LazyLock::new(|| std::env::var_os("VICTORY_PC_CONTROLS").is_some_and(|s| s == "1"));
    *ENABLED
}

fn captured(hwnd: HWND) -> bool {
    !hwnd.is_null() && CAPTURED.load(Ordering::Relaxed) == hwnd as usize
}

pub(super) fn pad(keys: &[KeyState], mouse: PadState) -> PadState {
    let mut pad = fallback(keys);
    // In PC mode F enters a vehicle; Ctrl takes over the stick click.
    pad.buttons &= !LEFT_STICK;
    for key in keys {
        match key.virtual_key {
            b'F' => pad.buttons |= Y,
            0x20 => pad.buttons |= X,                 // Space
            0xa0 | 0xa1 => pad.buttons |= A,          // Left/right Shift
            0xa2 | 0xa3 => pad.buttons |= LEFT_STICK, // Left/right Ctrl
            _ => {}
        }
    }
    merged(pad, mouse)
}

#[derive(Default)]
struct Motion {
    dx: i32,
    dy: i32,
    left: bool,
    right: bool,
    last_poll: Option<Instant>,
    absolute: Option<(usize, bool, i32, i32)>,
    previous_cursor: usize,
}

static MOTION: LazyLock<Mutex<Motion>> = LazyLock::new(|| Mutex::new(Motion::default()));

impl Motion {
    fn displacement(&mut self, device: usize, flags: u16, x: i32, y: i32) -> (i32, i32) {
        if flags & MOUSE_MOVE_ABSOLUTE == 0 {
            self.absolute = None;
            return (x, y);
        }
        // Tablets and remote desktops can report normalized screen positions.
        let virtual_desktop = flags & windows_sys::Win32::UI::Input::MOUSE_VIRTUAL_DESKTOP != 0;
        let previous = self.absolute.replace((device, virtual_desktop, x, y));
        let Some((old_device, old_desktop, old_x, old_y)) = previous else {
            return (0, 0);
        };
        if old_device != device || old_desktop != virtual_desktop {
            return (0, 0);
        }
        let (width, height) = if virtual_desktop {
            (wm::SM_CXVIRTUALSCREEN, wm::SM_CYVIRTUALSCREEN)
        } else {
            (wm::SM_CXSCREEN, wm::SM_CYSCREEN)
        };
        let pixels = |delta: i32, extent| {
            // SAFETY: reads the desktop dimensions for the reported coordinate space.
            (f64::from(delta) * f64::from(unsafe { wm::GetSystemMetrics(extent) }) / 65535.0)
                .round() as i32
        };
        (
            pixels(x.saturating_sub(old_x), width),
            pixels(y.saturating_sub(old_y), height),
        )
    }

    fn packet(&mut self, dx: i32, dy: i32, buttons: u32) {
        self.dx = self.dx.saturating_add(dx);
        self.dy = self.dy.saturating_add(dy);
        for (held, down, up) in [
            (
                &mut self.left,
                wm::RI_MOUSE_LEFT_BUTTON_DOWN,
                wm::RI_MOUSE_LEFT_BUTTON_UP,
            ),
            (
                &mut self.right,
                wm::RI_MOUSE_RIGHT_BUTTON_DOWN,
                wm::RI_MOUSE_RIGHT_BUTTON_UP,
            ),
        ] {
            if buttons & (down | up) != 0 {
                *held = buttons & up == 0;
            }
        }
    }

    fn sample(&mut self, now: Instant) -> PadState {
        let previous = self.last_poll.replace(now).unwrap_or(now);
        let (right_x, right_y) = stick(
            self.dx,
            self.dy.saturating_neg(),
            now.duration_since(previous).as_secs_f32(),
        );
        self.dx = 0;
        self.dy = 0;
        PadState {
            right_x,
            right_y,
            left_trigger: u8::from(self.right) as f32,
            right_trigger: u8::from(self.left) as f32,
            ..PadState::default()
        }
    }
}

pub(super) fn mouse_pad(hwnd: HWND) -> PadState {
    if captured(hwnd) {
        super::lock(&MOTION).sample(Instant::now())
    } else {
        PadState::default()
    }
}

fn register(hwnd: HWND, flags: u32) -> bool {
    // Generic desktop mouse only. No INPUTSINK: another app's input stays there.
    let device = RAWINPUTDEVICE {
        usUsagePage: 1,
        usUsage: 2,
        dwFlags: flags,
        hwndTarget: hwnd,
    };
    // SAFETY: the one device registration has the advertised size.
    unsafe { RegisterRawInputDevices(&device, 1, size_of::<RAWINPUTDEVICE>() as u32) != 0 }
}

fn release() {
    CAPTURED.store(0, Ordering::Relaxed);
    let previous = std::mem::take(&mut *super::lock(&MOTION)).previous_cursor;
    register(std::ptr::null_mut(), RIDEV_REMOVE);
    // SAFETY: restore the cursor previously returned on this window thread.
    unsafe { wm::SetCursor(previous as _) };
    tracing::info!("PC mouse released");
}

pub(in crate::modules) fn window_message(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> Option<LRESULT> {
    if !enabled() {
        return None;
    }
    match message {
        wm::WM_KEYDOWN if wparam == usize::from(VK_F6) && lparam & (1 << 30) == 0 => {
            // SAFETY: read-only query of the foreground window.
            let foreground = unsafe { GetForegroundWindow() };
            if captured(hwnd) {
                release();
            } else if foreground == hwnd {
                if register(hwnd, RIDEV_NOLEGACY | RIDEV_CAPTUREMOUSE) {
                    // SAFETY: this thread owns the game's window and cursor.
                    let previous_cursor = unsafe { wm::SetCursor(std::ptr::null_mut()) } as usize;
                    *super::lock(&MOTION) = Motion {
                        previous_cursor,
                        ..Motion::default()
                    };
                    CAPTURED.store(hwnd as usize, Ordering::Relaxed);
                    tracing::info!("PC mouse captured");
                } else {
                    tracing::warn!(error = %std::io::Error::last_os_error(), "could not register PC mouse");
                }
            }
            Some(0)
        }
        _ if !captured(hwnd) => None,
        wm::WM_KILLFOCUS | wm::WM_NCDESTROY => {
            release();
            None
        }
        wm::WM_ACTIVATEAPP if wparam == 0 => {
            release();
            None
        }
        wm::WM_SETCURSOR if lparam as u16 == wm::HTCLIENT as u16 => {
            // SAFETY: only the captured game client's cursor.
            unsafe { wm::SetCursor(std::ptr::null_mut()) };
            Some(1)
        }
        wm::WM_INPUT => {
            let mut raw = RAWINPUT::default();
            let mut bytes = size_of::<RAWINPUT>() as u32;
            // SAFETY: WM_INPUT supplies the handle; raw is aligned and holds a full mouse record.
            let got = unsafe {
                GetRawInputData(
                    lparam as _,
                    RID_INPUT,
                    (&raw mut raw).cast(),
                    &mut bytes,
                    size_of::<RAWINPUTHEADER>() as u32,
                )
            };
            if got == size_of::<RAWINPUT>() as u32 && raw.header.dwType == RIM_TYPEMOUSE {
                // SAFETY: checked raw input type. The button flags and data share a tagged mouse record.
                let (mouse, buttons) = unsafe {
                    (
                        raw.data.mouse,
                        raw.data.mouse.Anonymous.Anonymous.usButtonFlags,
                    )
                };
                let mut motion = super::lock(&MOTION);
                let (dx, dy) = motion.displacement(
                    raw.header.hDevice as usize,
                    mouse.usFlags,
                    mouse.lLastX,
                    mouse.lLastY,
                );
                motion.packet(dx, dy, buttons.into());
                drop(motion);
                // SAFETY: Win32 requires default cleanup for consumed foreground WM_INPUT records.
                return Some(unsafe { wm::DefWindowProcW(hwnd, message, wparam, lparam) });
            }
            None
        }
        _ => None,
    }
}

fn stick(dx: i32, dy: i32, seconds: f32) -> (f32, f32) {
    // At 900 relative mouse counts per second the mouse reaches full stick deflection.
    let scale = seconds.clamp(0.004, 0.1) * 900.0;
    (
        (dx as f32 / scale).clamp(-1.0, 1.0),
        (dy as f32 / scale).clamp(-1.0, 1.0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pc_actions_and_mouse_combine_with_keyboard_axes() {
        let key = |vk| KeyState {
            virtual_key: vk,
            ..Default::default()
        };
        let mouse = PadState {
            left_trigger: 1.0,
            right_trigger: 1.0,
            right_y: -0.75,
            ..Default::default()
        };
        let p = pad(&[key(b'W'), key(b'F'), key(0x20), key(0xa0)], mouse);
        assert_eq!(p.buttons, X | Y | A);
        assert_eq!(
            (p.left_y, p.right_y, p.left_trigger, p.right_trigger),
            (1.0, -0.75, 1.0, 1.0)
        );
        assert_eq!(
            pad(&[key(0xa2), key(b'F')], PadState::default()).buttons,
            LEFT_STICK | Y
        );
        assert_eq!(pad(&[], PadState::default()), PadState::default());
    }

    #[test]
    fn mouse_rate_has_direction_saturation_and_a_neutral_rest() {
        assert_eq!(stick(9, -9, 0.02), (0.5, -0.5));
        assert_eq!(stick(100, -100, 0.008), (1.0, -1.0));
        assert_eq!(stick(0, 0, 0.0), (0.0, 0.0));
    }

    #[test]
    fn packets_accumulate_between_polls_and_releases_clear_buttons() {
        let mut motion = Motion::default();
        let now = Instant::now();
        motion.sample(now);
        motion.packet(4, -3, wm::RI_MOUSE_RIGHT_BUTTON_DOWN);
        motion.packet(5, -6, wm::RI_MOUSE_LEFT_BUTTON_DOWN);
        let p = motion.sample(now + std::time::Duration::from_millis(20));
        assert_eq!(
            (p.right_x, p.right_y, p.left_trigger, p.right_trigger),
            (0.5, 0.5, 1.0, 1.0)
        );
        motion.packet(0, 0, wm::RI_MOUSE_LEFT_BUTTON_UP);
        let p = motion.sample(now + std::time::Duration::from_millis(40));
        assert_eq!(
            (p.right_x, p.right_y, p.left_trigger, p.right_trigger),
            (0., 0., 1., 0.)
        );
        motion.packet(0, 0, wm::RI_MOUSE_RIGHT_BUTTON_UP);
        assert_eq!(
            motion.sample(now + std::time::Duration::from_millis(60)),
            PadState::default()
        );
    }

    #[test]
    fn first_absolute_packet_is_an_anchor_and_relative_packets_reset_it() {
        let mut motion = Motion::default();
        assert_eq!(
            motion.displacement(1, MOUSE_MOVE_ABSOLUTE, 43000, 22000),
            (0, 0)
        );
        assert_eq!(
            motion.displacement(1, MOUSE_MOVE_ABSOLUTE, 43000, 22000),
            (0, 0)
        );
        assert_eq!(motion.displacement(2, MOUSE_MOVE_ABSOLUTE, 10, 20), (0, 0));
        assert_eq!(motion.displacement(2, 0, 16, -32), (16, -32));
        assert_eq!(
            motion.displacement(2, MOUSE_MOVE_ABSOLUTE, 65535, 0),
            (0, 0)
        );
    }
}
