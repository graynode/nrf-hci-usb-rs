//! RGB LED driver for the Thingy:53 — PWM-controlled.
//!
//! Hardware mapping (from board DTS, pwm0, PWM_POLARITY_NORMAL, active-high
//! transistor switches):
//!   ch0 / Red   → P1.08
//!   ch1 / Green → P1.06
//!   ch2 / Blue  → P1.07
//!
//! A global [`LED_STATE`] signal lets any task request a colour change.
//! The [`led_task`] owns the PWM peripheral and applies changes immediately.

use embassy_nrf::peripherals::{P1_06, P1_07, P1_08, PWM0};
use embassy_nrf::pwm::{DutyCycle, SimplePwm, SimpleConfig, Prescaler};
use embassy_nrf::Peri;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;

// ── Brightness ───────────────────────────────────────────────────────────── //

/// PWM top value (= 1kHz with Div16 prescaler on 16 MHz clock).
const MAX_DUTY: u16 = 1000;

/// Active brightness level — 8% of full scale keeps the LED comfortable
/// to look at directly.
const BRIGHTNESS: u16 = 80;

// ── Public colour vocabulary ─────────────────────────────────────────────── //

#[allow(dead_code)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum LedColor {
    Off,
    Red,
    Green,
    Blue,
    Yellow,   // Red + Green
    Cyan,     // Green + Blue
    Magenta,  // Red + Blue
    White,
}

// ── Global signal ─────────────────────────────────────────────────────────── //

/// Any task may write here to change the LED colour immediately.
pub static LED_STATE: Signal<CriticalSectionRawMutex, LedColor> = Signal::new();

// ── LED task ──────────────────────────────────────────────────────────────── //

#[embassy_executor::task]
pub async fn led_task(
    pwm: Peri<'static, PWM0>,
    r: Peri<'static, P1_08>,
    g: Peri<'static, P1_06>,
    b: Peri<'static, P1_07>,
) -> ! {
    // Div16 prescaler: 16 MHz / 16 = 1 MHz; 1 MHz / 1000 = 1 kHz PWM.
    let mut config = SimpleConfig::default();
    config.prescaler = Prescaler::Div16;
    config.max_duty = MAX_DUTY;

    // ch0 = Red, ch1 = Green, ch2 = Blue (matches DTS pwm0 channel order).
    let mut pwm = SimplePwm::new_3ch(pwm, r, g, b, &config);

    // Start with all channels off.
    set_color(&mut pwm, LedColor::Off);

    loop {
        let color = LED_STATE.wait().await;
        set_color(&mut pwm, color);
    }
}

fn set_color(pwm: &mut SimplePwm<'_>, color: LedColor) {
    let (r, g, b) = channel_bits(color);
    // DutyCycle::inverted: output is high when counter < value.
    // inverted(0)          = always low  = off
    // inverted(BRIGHTNESS) = BRIGHTNESS/MAX_DUTY on-time = dim
    pwm.set_duty(0, DutyCycle::inverted(if r { BRIGHTNESS } else { 0 }));
    pwm.set_duty(1, DutyCycle::inverted(if g { BRIGHTNESS } else { 0 }));
    pwm.set_duty(2, DutyCycle::inverted(if b { BRIGHTNESS } else { 0 }));
}

fn channel_bits(c: LedColor) -> (bool, bool, bool) {
    match c {
        LedColor::Off     => (false, false, false),
        LedColor::Red     => (true,  false, false),
        LedColor::Green   => (false, true,  false),
        LedColor::Blue    => (false, false, true),
        LedColor::Yellow  => (true,  true,  false),
        LedColor::Cyan    => (false, true,  true),
        LedColor::Magenta => (true,  false, true),
        LedColor::White   => (true,  true,  true),
    }
}

