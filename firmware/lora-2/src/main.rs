#![no_std]
#![no_main]

mod iv;

use defmt::*;
use embassy_executor::Spawner;
use embassy_stm32::{
    bind_interrupts,
    gpio::{Level, Output, Pin, Speed},
    i2c::{Config as I2cConfig, I2c},
    peripherals::{self, I2C2, PA11, PA12},
    rcc::*,
    rng::{self, Rng},
    spi::Spi,
    time::Hertz,
    Config,
};
use embassy_time::{Delay, Timer};
use embedded_graphics::{
    mono_font::{ascii::FONT_6X10, MonoTextStyle},
    pixelcolor::BinaryColor,
    prelude::*,
    text::Text,
};
use sh1106::{prelude::*, Builder};
use {defmt_rtt as _, panic_probe as _};

use lora_phy::lorawan_radio::LorawanRadio;
use lora_phy::sx126x::{self, Stm32wl, Sx126x, TcxoCtrlVoltage};
use lora_phy::LoRa;
use lorawan_device::async_device::{region, Device, EmbassyTimer, JoinMode, JoinResponse, SendResponse};
use lorawan_device::default_crypto::DefaultFactory;
use lorawan_device::region::{Subband, AU915};
use lorawan_device::{AppEui, AppKey, DevEui};

use self::iv::{InterruptHandler, Stm32wlInterfaceVariant, SubghzSpiDevice};

bind_interrupts!(struct Irqs {
    SUBGHZ_RADIO => InterruptHandler;
    RNG => rng::InterruptHandler<peripherals::RNG>;
});

const MAX_TX_POWER: u8 = 14;

const DEV_EUI: [u8; 8] = [0xAC, 0x1F, 0x09, 0xFF, 0xFE, 0x1B, 0xCE, 0x24];
const APP_EUI: [u8; 8] = [0x56, 0x53, 0x29, 0xC5, 0x64, 0xA8, 0x30, 0xB1];
const APP_KEY: [u8; 16] = [
    0xB7, 0x26, 0x73, 0x9B, 0x78, 0xEC, 0x4B, 0x9E,
    0x92, 0x34, 0xE5, 0xD3, 0x5E, 0xA9, 0x68, 0x1B,
];

fn update_display(
    tx_count: u32,
    is_joined: bool,
    join_attempt: u32,
    snr: i8,
    rssi: i16,
    dr: u8,
    text_style: MonoTextStyle<BinaryColor>,
) {
    let mut i2c_cfg = I2cConfig::default();
    i2c_cfg.sda_pullup = true;
    i2c_cfg.scl_pullup = true;
    let i2c = unsafe {
        I2c::new_blocking(I2C2::steal(), PA12::steal(), PA11::steal(), Hertz(100_000), i2c_cfg)
    };

    let mut disp: GraphicsMode<_> = Builder::new()
        .with_size(DisplaySize::Display128x64)
        .connect_i2c(i2c)
        .into();

    disp.clear();

    // Line 1: join status or tx count
    let mut line = heapless::String::<32>::new();
    if is_joined {
        let _ = core::fmt::write(&mut line, format_args!("LoRa-2     Tx:{:>4}  ", tx_count));
    } else {
        let _ = core::fmt::write(&mut line, format_args!("LoRa-2   Join:{:>2}    ", join_attempt));
    }
    let _ = Text::new(&line, Point::new(0, 10), text_style).draw(&mut disp);

    // Line 2: RSSI
    line.clear();
    if is_joined {
        let _ = core::fmt::write(&mut line, format_args!("RSSI: {:>4} dBm       ", rssi));
    } else {
        let _ = core::fmt::write(&mut line, format_args!("                    "));
    }
    let _ = Text::new(&line, Point::new(0, 22), text_style).draw(&mut disp);

    // Line 3: SNR
    line.clear();
    if is_joined {
        let _ = core::fmt::write(&mut line, format_args!("SNR:  {:>+4} dB        ", snr));
    } else {
        let _ = core::fmt::write(&mut line, format_args!("                    "));
    }
    let _ = Text::new(&line, Point::new(0, 34), text_style).draw(&mut disp);

    // Line 4: DR / SF
    line.clear();
    if is_joined {
        let sf = 12u8.saturating_sub(dr);
        let _ = core::fmt::write(&mut line, format_args!("DR{} SF{}              ", dr, sf));
    } else {
        let _ = core::fmt::write(&mut line, format_args!("                    "));
    }
    let _ = Text::new(&line, Point::new(0, 46), text_style).draw(&mut disp);

    // Line 5: status
    line.clear();
    if is_joined {
        let _ = core::fmt::write(&mut line, format_args!("Connected           "));
    } else {
        let _ = core::fmt::write(&mut line, format_args!("Connecting...       "));
    }
    let _ = Text::new(&line, Point::new(0, 58), text_style).draw(&mut disp);

    let _ = disp.flush();
}

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    info!("====================================");
    info!("  STM32WL55 LoRa-2 - Range Tester");
    info!("====================================");

    let mut config = Config::default();
    {
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
    }
    let p = embassy_stm32::init(config);

    info!("Clock initialized");

    // Radio
    let ctrl1 = Output::new(p.PC4.degrade(), Level::Low, Speed::High);
    let ctrl2 = Output::new(p.PC5.degrade(), Level::Low, Speed::High);
    let ctrl3 = Output::new(p.PC3.degrade(), Level::High, Speed::High);

    let spi = Spi::new_subghz(p.SUBGHZSPI, p.DMA1_CH1, p.DMA1_CH2);
    let spi = SubghzSpiDevice(spi);

    let use_high_power_pa = true;
    let radio_config = sx126x::Config {
        chip: Stm32wl { use_high_power_pa },
        tcxo_ctrl: Some(TcxoCtrlVoltage::Ctrl1V7),
        use_dcdc: true,
        rx_boost: false,
    };

    let iv = Stm32wlInterfaceVariant::new(Irqs, use_high_power_pa, Some(ctrl1), Some(ctrl2), Some(ctrl3)).unwrap();
    let lora = LoRa::new(Sx126x::new(spi, iv, radio_config), true, Delay).await.unwrap();
    let radio: LorawanRadio<_, _, MAX_TX_POWER> = lora.into();

    let mut au915 = AU915::new();
    au915.set_join_bias_and_noncompliant_retries(Subband::_1, 20);
    let region: region::Configuration = au915.into();

    let rng = Rng::new(p.RNG, Irqs);
    let mut device: Device<_, DefaultFactory, _, _> = Device::new(region, radio, EmbassyTimer::new(), rng);

    let mut led = Output::new(p.PB15, Level::Low, Speed::Low);

    // Initialise display once at startup — no re-init in the loop to avoid flicker.
    {
        let mut i2c_cfg = I2cConfig::default();
        i2c_cfg.sda_pullup = true;
        i2c_cfg.scl_pullup = true;
        let i2c = unsafe {
            I2c::new_blocking(I2C2::steal(), PA12::steal(), PA11::steal(), Hertz(100_000), i2c_cfg)
        };
        let mut disp: GraphicsMode<_> = Builder::new()
            .with_size(DisplaySize::Display128x64)
            .connect_i2c(i2c)
            .into();
        let _ = disp.init();
        disp.clear();
        let _ = disp.flush();
    }

    let text_style = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);

    let mut tx_count = 0u32;
    let mut uplink_counter = 0u32;
    let mut snr = 0i8;
    let mut rssi = 0i16;
    let mut dr: u8 = 0;
    let mut is_joined = false;
    let mut join_attempt = 0u32;
    let mut uplink_fail_count = 0u8;
    let mut join_backoff_ticks: u32 = 8;
    let mut join_wait_counter: u32 = 0;

    const UPLINK_INTERVAL: u32 = 5;       // send every ~10s (5 ticks × 2s)
    const MAX_UPLINK_FAILS: u8 = 3;
    const JOIN_BACKOFF_MAX: u32 = 30;

    let join_mode = JoinMode::OTAA {
        deveui: DevEui::from(DEV_EUI),
        appeui: AppEui::from(APP_EUI),
        appkey: AppKey::from(APP_KEY),
    };

    info!("DevEUI: {:02X}", DEV_EUI);

    // Initial join
    join_attempt += 1;
    update_display(tx_count, is_joined, join_attempt, snr, rssi, dr, text_style);

    match device.join(&join_mode).await {
        Ok(JoinResponse::JoinSuccess) => {
            info!("✓ Joined!");
            is_joined = true;
            join_backoff_ticks = 8;
        }
        Ok(JoinResponse::NoJoinAccept) => error!("✗ No join accept"),
        Err(e) => error!("✗ Join error: {:?}", e),
    }

    update_display(tx_count, is_joined, join_attempt, snr, rssi, dr, text_style);

    info!("Starting main loop...");

    loop {
        uplink_counter += 1;

        if is_joined {
            if uplink_counter >= UPLINK_INTERVAL {
                uplink_counter = 0;

                // Payload: just a counter so we can track packet loss
                let payload: [u8; 4] = [
                    (tx_count >> 24) as u8,
                    (tx_count >> 16) as u8,
                    (tx_count >> 8) as u8,
                    tx_count as u8,
                ];

                led.set_high();

                // Always confirmed — this is a range probe; every packet must be ACK'd
                // so gateway loss is detected within MAX_UPLINK_FAILS uplink cycles.
                match device.send(&payload, 1, true).await {
                    Ok(SendResponse::NoAck) => {
                        uplink_fail_count += 1;
                        error!("✗ No ACK ({}/{})", uplink_fail_count, MAX_UPLINK_FAILS);
                        if uplink_fail_count >= MAX_UPLINK_FAILS {
                            is_joined = false;
                            join_attempt = 0;
                            uplink_fail_count = 0;
                            join_backoff_ticks = 8;
                            join_wait_counter = 0;
                        }
                    }
                    Ok(response) => {
                        tx_count += 1;
                        snr = device.last_snr() as i8;
                        rssi = device.last_rssi();
                        dr = device.get_datarate() as u8;
                        uplink_fail_count = 0;
                        info!("✓ Tx#{} | DR{} SF{} | SNR:{} RSSI:{} | {:?}",
                            tx_count, dr, 12u8.saturating_sub(dr), snr, rssi, response);
                    }
                    Err(e) => {
                        uplink_fail_count += 1;
                        error!("✗ Tx error ({}/{}): {:?}", uplink_fail_count, MAX_UPLINK_FAILS, e);
                        if uplink_fail_count >= MAX_UPLINK_FAILS {
                            is_joined = false;
                            join_attempt = 0;
                            uplink_fail_count = 0;
                            join_backoff_ticks = 8;
                            join_wait_counter = 0;
                        }
                    }
                }

                led.set_low();
            }
        } else {
            join_wait_counter += 1;
            if join_wait_counter >= join_backoff_ticks {
                join_wait_counter = 0;
                join_attempt += 1;
                info!("Join attempt {} (backoff={}s)...", join_attempt, join_backoff_ticks * 2);

                match device.join(&join_mode).await {
                    Ok(JoinResponse::JoinSuccess) => {
                        info!("✓ Joined!");
                        is_joined = true;
                        uplink_fail_count = 0;
                        join_backoff_ticks = 8;
                    }
                    Ok(JoinResponse::NoJoinAccept) => {
                        join_backoff_ticks = (join_backoff_ticks * 2).min(JOIN_BACKOFF_MAX);
                        error!("✗ No join accept - retry in {}s", join_backoff_ticks * 2);
                    }
                    Err(e) => {
                        join_backoff_ticks = (join_backoff_ticks * 2).min(JOIN_BACKOFF_MAX);
                        error!("✗ Join error: {:?} - retry in {}s", e, join_backoff_ticks * 2);
                    }
                }
            }
        }

        update_display(tx_count, is_joined, join_attempt, snr, rssi, dr, text_style);
        Timer::after_secs(2).await;
    }
}
