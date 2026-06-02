//! Direct uinput **absolute** pointer.
//!
//! ydotool's virtual device is relative-only (`EV=7`: SYN|KEY|REL), so its
//! `--absolute` is faked as "pin-to-corner + relative move", which the
//! compositor then distorts with pointer acceleration and fractional display
//! scaling — clicks land in the wrong place on multi-monitor / HiDPI setups.
//!
//! Here we create our own uinput device that exposes a true `ABS_X`/`ABS_Y`
//! axis whose range is set to the **logical desktop size** queried from the
//! compositor (not the physical pixel dimensions of the screenshot PNG).
//! The compositor normalises an absolute device's reported position across the
//! logical desktop, so callers must convert screenshot-space (physical pixel)
//! coordinates to logical space before calling [`AbsPointer::move_to`] or
//! [`AbsPointer::click`]. See `server::ensure_abs_pointer` for how the scale
//! factors are computed and applied.

use std::thread::sleep;
use std::time::Duration;

use anyhow::{Context, Result};
use evdev::{
    uinput::VirtualDevice, AbsInfo, AbsoluteAxisCode, AttributeSet, EventType, InputEvent, KeyCode,
    PropType, UinputAbsSetup,
};

pub struct AbsPointer {
    device: VirtualDevice,
    width: i32,
    height: i32,
}

impl AbsPointer {
    /// Create the absolute pointer sized to the logical desktop `width`×`height`
    /// (the portal screenshot dimensions). Blocks ~500 ms so libinput picks
    /// the device up before the first event.
    pub fn create(width: i32, height: i32) -> Result<Self> {
        build_device(width.max(1), height.max(1))
    }

    /// Recalibrate the axis range to `width`×`height`. If the dimensions are
    /// unchanged this is a no-op. When they differ, the uinput device is
    /// destroyed and recreated (ABS axis ranges cannot be updated on a live fd),
    /// which incurs another ~500 ms libinput settle delay.
    pub fn resize(&mut self, width: i32, height: i32) -> Result<()> {
        let width = width.max(1);
        let height = height.max(1);
        if self.width == width && self.height == height {
            return Ok(());
        }
        let new = build_device(width, height)?;
        self.device = new.device;
        self.width = new.width;
        self.height = new.height;
        Ok(())
    }

    /// Move the pointer to absolute logical coordinates `(x, y)`.
    pub fn move_to(&mut self, x: i32, y: i32) -> Result<()> {
        let x = x.clamp(0, self.width);
        let y = y.clamp(0, self.height);
        self.device
            .emit(&[
                InputEvent::new_now(EventType::ABSOLUTE.0, AbsoluteAxisCode::ABS_X.0, x),
                InputEvent::new_now(EventType::ABSOLUTE.0, AbsoluteAxisCode::ABS_Y.0, y),
            ])
            .context("failed to emit absolute motion")?;
        Ok(())
    }

    /// Move to `(x, y)` then press+release `button` `count` times.
    pub fn click(&mut self, x: i32, y: i32, button: PointerButton, count: u32) -> Result<()> {
        self.move_to(x, y)?;
        sleep(Duration::from_millis(30));
        let code = button.key_code();
        for _ in 0..count.max(1) {
            self.device
                .emit(&[InputEvent::new_now(EventType::KEY.0, code, 1)])?;
            sleep(Duration::from_millis(30));
            self.device
                .emit(&[InputEvent::new_now(EventType::KEY.0, code, 0)])?;
            sleep(Duration::from_millis(40));
        }
        Ok(())
    }

    /// Press at `(start)`, move to `(end)`, release — a drag with `button`.
    pub fn drag(
        &mut self,
        start: (i32, i32),
        end: (i32, i32),
        button: PointerButton,
    ) -> Result<()> {
        let code = button.key_code();
        self.move_to(start.0, start.1)?;
        sleep(Duration::from_millis(30));
        self.device
            .emit(&[InputEvent::new_now(EventType::KEY.0, code, 1)])?;
        sleep(Duration::from_millis(40));
        self.move_to(end.0, end.1)?;
        sleep(Duration::from_millis(40));
        self.device
            .emit(&[InputEvent::new_now(EventType::KEY.0, code, 0)])?;
        Ok(())
    }
}

fn build_device(width: i32, height: i32) -> Result<AbsPointer> {
    // value, min, max, fuzz, flat, resolution. resolution=1 unit/px.
    let abs_x =
        UinputAbsSetup::new(AbsoluteAxisCode::ABS_X, AbsInfo::new(0, 0, width, 0, 0, 1));
    let abs_y =
        UinputAbsSetup::new(AbsoluteAxisCode::ABS_Y, AbsInfo::new(0, 0, height, 0, 0, 1));
    let keys =
        AttributeSet::from_iter([KeyCode::BTN_LEFT, KeyCode::BTN_RIGHT, KeyCode::BTN_MIDDLE]);
    // INPUT_PROP_DIRECT marks the device as a direct (absolute) pointer so
    // libinput maps its axes to screen coordinates rather than treating it
    // as a relative touchpad.
    let props = AttributeSet::from_iter([PropType::DIRECT]);

    let device = VirtualDevice::builder()
        .context("uinput builder (is /dev/uinput writable?)")?
        .name("codex-computer-use-linux absolute pointer")
        .with_properties(&props)?
        .with_absolute_axis(&abs_x)?
        .with_absolute_axis(&abs_y)?
        .with_keys(&keys)?
        .build()
        .context("failed to create uinput absolute pointer device")?;

    // Give udev/libinput time to enumerate the new device.
    sleep(Duration::from_millis(500));

    Ok(AbsPointer {
        device,
        width,
        height,
    })
}

/// Pointer buttons we can synthesize.
#[derive(Clone, Copy, Debug)]
pub enum PointerButton {
    Left,
    Right,
    Middle,
}

impl PointerButton {
    pub fn from_name(name: Option<&str>) -> Self {
        match name.unwrap_or("left").to_ascii_lowercase().as_str() {
            "right" => Self::Right,
            "middle" => Self::Middle,
            _ => Self::Left,
        }
    }

    fn key_code(self) -> u16 {
        match self {
            Self::Left => KeyCode::BTN_LEFT.0,
            Self::Right => KeyCode::BTN_RIGHT.0,
            Self::Middle => KeyCode::BTN_MIDDLE.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_name_defaults_to_left() {
        assert!(matches!(PointerButton::from_name(None), PointerButton::Left));
        assert!(matches!(
            PointerButton::from_name(Some("unknown")),
            PointerButton::Left
        ));
    }

    #[test]
    fn from_name_recognises_right_and_middle() {
        assert!(matches!(
            PointerButton::from_name(Some("right")),
            PointerButton::Right
        ));
        assert!(matches!(
            PointerButton::from_name(Some("RIGHT")),
            PointerButton::Right
        ));
        assert!(matches!(
            PointerButton::from_name(Some("middle")),
            PointerButton::Middle
        ));
    }

    #[test]
    fn build_device_clamps_zero_dims_to_one() {
        // build_device requires /dev/uinput; skip if unavailable.
        if !std::path::Path::new("/dev/uinput").exists() {
            return;
        }
        let p = crate::abs_pointer::AbsPointer::create(0, 0);
        if let Ok(p) = p {
            assert_eq!(p.width, 1);
            assert_eq!(p.height, 1);
        }
    }
}
