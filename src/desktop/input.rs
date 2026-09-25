//! Remote input enters the compositor's normal input path, in process.
use std::collections::BTreeSet;
use std::sync::atomic::Ordering;

use anyhow::{ensure, Result};
use rho_desktop_proto::Input;
use smithay::backend::input::{Axis, AxisSource, ButtonState, KeyState, MouseButton};
use smithay::input::pointer::AxisFrame;

use super::video::Quality;
use crate::niri::State;

#[derive(Default)]
pub struct Held {
    keys: BTreeSet<u32>,
    buttons: BTreeSet<u32>,
    feedback_id: Option<u64>,
}

pub fn apply(state: &mut State, held: &mut Held, quality: &Quality, input: Input) -> Result<()> {
    let time = crate::utils::get_monotonic_time().as_millis() as u32;
    if !matches!(input, Input::Feedback(_)) {
        state.niri.notify_activity();
    }
    match input {
        Input::Move { x, y } => {
            let output = state
                .niri
                .global_space
                .outputs()
                .next()
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("no output"))?;
            let scale = output.current_scale().fractional_scale();
            let size = output
                .current_transform()
                .transform_size(output.current_mode().unwrap().size);
            let geo = state.niri.global_space.output_geometry(&output).unwrap();
            let pos = smithay::utils::Point::from((
                x.min(size.w as u32 - 1) as f64 / scale,
                y.min(size.h as u32 - 1) as f64 / scale,
            )) + geo.loc.to_f64();
            state.desktop_motion(pos, time);
        }
        Input::Button { button, pressed } => {
            ensure!(
                (0x110..=0x114).contains(&button),
                "unsupported pointer button"
            );
            if pressed {
                held.buttons.insert(button);
            } else {
                held.buttons.remove(&button);
            }
            let named = match button {
                0x110 => Some(MouseButton::Left),
                0x111 => Some(MouseButton::Right),
                0x112 => Some(MouseButton::Middle),
                0x113 => Some(MouseButton::Back),
                0x114 => Some(MouseButton::Forward),
                _ => None,
            };
            state.desktop_button(
                named,
                button,
                if pressed {
                    ButtonState::Pressed
                } else {
                    ButtonState::Released
                },
                time,
            );
        }
        Input::Physical { code, pressed } => {
            ensure!(code <= 767, "invalid evdev key");
            if pressed {
                held.keys.insert(code);
            } else {
                held.keys.remove(&code);
            }
            state.desktop_key(
                (code + 8).into(),
                if pressed {
                    KeyState::Pressed
                } else {
                    KeyState::Released
                },
                time,
                &mut false,
            );
        }
        Input::Scroll {
            horizontal,
            vertical,
        } => {
            ensure!(
                horizontal.is_finite() && vertical.is_finite(),
                "invalid scroll delta"
            );
            let pointer = state.niri.seat.get_pointer().unwrap();
            pointer.axis(
                state,
                AxisFrame::new(time)
                    .source(AxisSource::Wheel)
                    .value(Axis::Horizontal, horizontal.clamp(-4096., 4096.))
                    .value(Axis::Vertical, vertical.clamp(-4096., 4096.)),
            );
            pointer.frame(state);
        }
        Input::ReleaseAll => {
            for code in std::mem::take(&mut held.keys) {
                state.desktop_key((code + 8).into(), KeyState::Released, time, &mut false);
            }
            for code in std::mem::take(&mut held.buttons) {
                state.desktop_button(None, code, ButtonState::Released, time);
            }
        }
        Input::Feedback(feedback) => {
            quality.feedback(&mut held.feedback_id, feedback, std::time::Instant::now());
            if feedback.recover {
                super::video::request_recovery(state, quality)?;
            }
        }
        Input::Quality { bitrate, keyframe } => {
            quality
                .bitrate
                .store(bitrate.clamp(128_000, 20_000_000), Ordering::Release);
            if keyframe {
                super::video::request_recovery(state, quality)?;
            }
        }
        Input::Text(text) => {
            ensure!(text.len() <= 16384, "text input too large");
            for c in text.chars() {
                text_character(state, c, time)?;
            }
        }
        Input::Key(chord) => {
            let mut rest = chord.as_str();
            let mut mods = Vec::new();
            while let Some(i) = rest.find(['+', '-']) {
                let code = match &rest[..i] {
                    "ctrl" | "control" => 29,
                    "shift" => 42,
                    "alt" => 56,
                    "super" | "meta" | "logo" => 125,
                    _ => break,
                };
                mods.push(code);
                rest = &rest[i + 1..];
            }
            let code = match rest.to_ascii_lowercase().as_str() {
                "enter" | "return" => 28,
                "escape" | "esc" => 1,
                "tab" => 15,
                "backspace" => 14,
                "delete" | "del" => 111,
                "space" => 57,
                "up" => 103,
                "down" => 108,
                "left" => 105,
                "right" => 106,
                "home" => 102,
                "end" => 107,
                "pageup" => 104,
                "pagedown" => 109,
                _ => {
                    ascii_key(
                        rest.chars()
                            .next()
                            .filter(|_| rest.chars().count() == 1)
                            .ok_or_else(|| anyhow::anyhow!("unsupported key {rest}"))?,
                    )?
                    .0
                }
            };
            for &m in &mods {
                key(state, m, true, time);
            }
            key(state, code, true, time);
            key(state, code, false, time);
            for m in mods.into_iter().rev() {
                key(state, m, false, time);
            }
        }
    }
    Ok(())
}
pub fn disconnect(held: &Held, quality: &Quality) {
    quality.remove_viewer(held.feedback_id);
}
fn key(state: &mut State, code: u32, down: bool, time: u32) {
    state.desktop_key(
        (code + 8).into(),
        if down {
            KeyState::Pressed
        } else {
            KeyState::Released
        },
        time,
        &mut false,
    );
}
fn ascii_key(c: char) -> Result<(u32, bool)> {
    const ROWS: [(&str, &str, u32); 4] = [
        ("1234567890-=", "!@#$%^&*()_+", 2),
        ("qwertyuiop[]", "QWERTYUIOP{}", 16),
        ("asdfghjkl;'", "ASDFGHJKL:\"", 30),
        ("zxcvbnm,./", "ZXCVBNM<>?", 44),
    ];
    match c {
        ' ' => return Ok((57, false)),
        '\n' | '\r' => return Ok((28, false)),
        '\t' => return Ok((15, false)),
        '`' => return Ok((41, false)),
        '~' => return Ok((41, true)),
        '\\' => return Ok((43, false)),
        '|' => return Ok((43, true)),
        _ => {}
    }
    for (base, shift, start) in ROWS {
        if let Some(i) = base.chars().position(|v| v == c) {
            return Ok((start + i as u32, false));
        }
        if let Some(i) = shift.chars().position(|v| v == c) {
            return Ok((start + i as u32, true));
        }
    }
    anyhow::bail!("character not in US keymap: {c}")
}
fn text_character(state: &mut State, c: char, time: u32) -> Result<()> {
    let Ok((code, shift)) = ascii_key(c) else {
        // A Unicode keysym need not exist in the current layout. Temporarily map
        // one key, emit its ordered press/release, then restore the exact keymap.
        let keyboard = state.niri.seat.get_keyboard().unwrap();
        let previous = keyboard.with_xkb_state(state, |context| {
            let xkb = context.xkb().lock().unwrap();
            // The mutex guards all access; the returned string owns its bytes.
            unsafe { xkb.keymap() }.get_as_string(1)
        });
        let keymap = format!(
            r#"xkb_keymap {{
            xkb_keycodes {{ minimum=8; maximum=255; <RHO>=255; }};
            xkb_types {{ include "complete" }};
            xkb_compatibility {{ include "complete" }};
            xkb_symbols {{ key <RHO> {{ [ U{:04X} ] }}; }};
        }};"#,
            c as u32
        );
        keyboard.set_keymap_from_string(state, keymap)?;
        key(state, 247, true, time);
        key(state, 247, false, time);
        keyboard.set_keymap_from_string(state, previous)?;
        return Ok(());
    };
    if shift {
        key(state, 42, true, time);
    }
    key(state, code, true, time);
    key(state, code, false, time);
    if shift {
        key(state, 42, false, time);
    }
    Ok(())
}
