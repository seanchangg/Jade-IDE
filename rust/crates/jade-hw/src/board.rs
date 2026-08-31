//! Board metadata for the simulated hardware.
//!
//! The only board today is the Altera MAX 10 FPGA Development Kit
//! (DK-DEV-10M50-A). The table maps FPGA pins to board signals. To add a
//! board later, add a new table.

/// A user-visible signal on the board.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoardSignal {
    /// CLK_50_MAX10, a 50 MHz clock input.
    Clock50,
    /// USER_LED[i]. The LED is on when the pin is low.
    Led(u8),
    /// USER_PB[i]. The pin is low when you push the button.
    Pb(u8),
    /// USER_DIPSW[i]. The pin is low when the switch is in the ON position.
    DipSw(u8),
}

/// One pin on the board.
#[derive(Debug, Clone, Copy)]
pub struct BoardPin {
    /// The FPGA pin name without the `PIN_` prefix, for example `T20`.
    pub pin: &'static str,
    pub signal: BoardSignal,
}

/// A development board that the simulator can model 1:1.
#[derive(Debug, Clone)]
pub struct BoardDef {
    pub name: &'static str,
    pub device: &'static str,
    pub clock_hz: u64,
    pub pins: &'static [BoardPin],
    pub led_count: u8,
    pub pb_count: u8,
    pub dipsw_count: u8,
}

/// Pin table for the DK-DEV-10M50-A (PCB 100-0321401 Rev C).
const DK_DEV_10M50A_PINS: &[BoardPin] = &[
    BoardPin { pin: "M9", signal: BoardSignal::Clock50 },
    BoardPin { pin: "T20", signal: BoardSignal::Led(0) },
    BoardPin { pin: "U22", signal: BoardSignal::Led(1) },
    BoardPin { pin: "U21", signal: BoardSignal::Led(2) },
    BoardPin { pin: "AA21", signal: BoardSignal::Led(3) },
    BoardPin { pin: "AA22", signal: BoardSignal::Led(4) },
    BoardPin { pin: "L22", signal: BoardSignal::Pb(0) },
    BoardPin { pin: "M21", signal: BoardSignal::Pb(1) },
    BoardPin { pin: "M22", signal: BoardSignal::Pb(2) },
    BoardPin { pin: "N21", signal: BoardSignal::Pb(3) },
    BoardPin { pin: "H21", signal: BoardSignal::DipSw(0) },
    BoardPin { pin: "H22", signal: BoardSignal::DipSw(1) },
    BoardPin { pin: "J21", signal: BoardSignal::DipSw(2) },
    BoardPin { pin: "J22", signal: BoardSignal::DipSw(3) },
    BoardPin { pin: "G19", signal: BoardSignal::DipSw(4) },
];

/// The MAX 10 FPGA Development Kit.
pub fn dk_dev_10m50a() -> BoardDef {
    BoardDef {
        name: "DK-DEV-10M50-A",
        device: "10M50DAF484C6GES",
        clock_hz: 50_000_000,
        pins: DK_DEV_10M50A_PINS,
        led_count: 5,
        pb_count: 4,
        dipsw_count: 5,
    }
}

impl BoardDef {
    /// Find the board signal for an FPGA pin name. Accept the name with or
    /// without the `PIN_` prefix, in any letter case.
    pub fn signal_for_pin(&self, pin: &str) -> Option<BoardSignal> {
        let name = pin.strip_prefix("PIN_").unwrap_or(pin);
        self.pins
            .iter()
            .find(|p| p.pin.eq_ignore_ascii_case(name))
            .map(|p| p.signal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_lookup_accepts_prefix_and_case() {
        let b = dk_dev_10m50a();
        assert_eq!(b.signal_for_pin("PIN_M9"), Some(BoardSignal::Clock50));
        assert_eq!(b.signal_for_pin("aa22"), Some(BoardSignal::Led(4)));
        assert_eq!(b.signal_for_pin("L22"), Some(BoardSignal::Pb(0)));
        assert_eq!(b.signal_for_pin("G19"), Some(BoardSignal::DipSw(4)));
        assert_eq!(b.signal_for_pin("Z1"), None);
    }
}
