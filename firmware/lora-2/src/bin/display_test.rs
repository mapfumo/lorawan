#![no_std]
#![no_main]

use defmt::*;
use embassy_executor::Spawner;
use embassy_stm32::{rcc::*, time::Hertz, Config};
use embassy_time::Timer;
use {defmt_rtt as _, panic_probe as _};

#[path = "../display.rs"]
mod display;

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    info!("Display test");

    let mut config = Config::default();
    config.rcc.hse = Some(Hse {
        freq: Hertz(32_000_000),
        mode: HseMode::Bypass,
        prescaler: HsePrescaler::DIV1,
    });
    config.rcc.sys = Sysclk::PLL1_R;
    config.rcc.pll = Some(Pll {
        source: PllSource::HSE,
        prediv: PllPreDiv::DIV2,
        mul: PllMul::MUL6,
        divp: None,
        divq: Some(PllQDiv::DIV2),
        divr: Some(PllRDiv::DIV2),
    });
    embassy_stm32::init(config);

    display::bus_recover();
    info!("bus recovered");

    // Wait for any device on the bus to fully settle after recovery
    cortex_m::asm::delay(480_000); // 10ms at 48MHz

    display::init();
    info!("init done");

    display::clear();
    info!("clear done");

    display::draw_text(0, 0, "LoRa-2   Tx:   0");
    display::draw_text(1, 0, "Temp: 28C");
    display::draw_text(2, 0, "Hum:  55%");
    display::draw_text(3, 0, "Press: 1022 hPa");
    display::draw_text(5, 0, "RSSI:-85  SNR:+4");
    display::draw_text(7, 0, "DR0 SF12");
    info!("draw done");

    loop {
        Timer::after_secs(5).await;
        info!("still alive");
    }
}
