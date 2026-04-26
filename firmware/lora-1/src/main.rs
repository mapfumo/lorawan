#![no_std]
#![no_main]

// LoRaWAN interface variant module for RF switch control
mod iv;

use defmt::*;
use embassy_executor::Spawner;
use embassy_stm32::{
    bind_interrupts,
    gpio::{Level, Output, Pin, Speed},
    i2c::{Config as I2cConfig, EventInterruptHandler, ErrorInterruptHandler, I2c},
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

// LoRaWAN imports (not used yet, but prepared)
use lora_phy::lorawan_radio::LorawanRadio;
use lora_phy::sx126x::{self, Stm32wl, Sx126x, TcxoCtrlVoltage};
use lora_phy::LoRa;
use lorawan_device::async_device::{region, Device, EmbassyTimer, JoinMode, JoinResponse, SendResponse};
use lorawan_device::default_crypto::DefaultFactory;
use lorawan_device::region::{Subband, AU915};
use lorawan_device::{AppEui, AppKey, DevEui};

use self::iv::{InterruptHandler, Stm32wlInterfaceVariant, SubghzSpiDevice};

bind_interrupts!(struct I2c2Irqs {
    I2C2_EV => EventInterruptHandler<peripherals::I2C2>;
    I2C2_ER => ErrorInterruptHandler<peripherals::I2C2>;
});

bind_interrupts!(struct Irqs{
    SUBGHZ_RADIO => InterruptHandler;
    RNG => rng::InterruptHandler<peripherals::RNG>;
});

// SHT41 sensor constants
const SHT41_ADDR: u8 = 0x44;
const CMD_MEASURE_HIGH_PRECISION: u8 = 0xFD;

// LoRaWAN configuration constants
const MAX_TX_POWER: u8 = 14; // AU915 max TX power

// LoRaWAN credentials (from gateway TOT application)
// Note: EUIs are stored in LITTLE-ENDIAN for over-the-air transmission
const DEV_EUI: [u8; 8] = [0xAC, 0x1F, 0x09, 0xFF, 0xFE, 0x1B, 0xCE, 0x23]; // 23ce1bfeff091fac reversed
const APP_EUI: [u8; 8] = [0x56, 0x53, 0x29, 0xC5, 0x64, 0xA8, 0x30, 0xB1]; // b130a864c5295356 reversed
const APP_KEY: [u8; 16] = [
    0xB7, 0x26, 0x73, 0x9B, 0x78, 0xEC, 0x4B, 0x9E,
    0x92, 0x34, 0xE5, 0xD3, 0x5E, 0xA9, 0x68, 0x1B,
]; // AppKey stays in big-endian (MSB first)

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    info!("====================================");
    info!("  STM32WL55 LoRa-1 - SHT41");
    info!("  Temperature & Humidity Sensor + LoRaWAN");
    info!("====================================");

    // Clock configuration matching working solution (HSE + PLL for radio stability)
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

    info!("STM32WL55 initialized with HSE + PLL clock");

    // Test I2C2 bus and detect devices
    {
        info!("Testing I2C2: PA12 (SCL), PA11 (SDA)");
        let mut i2c_config = I2cConfig::default();
        i2c_config.sda_pullup = true;
        i2c_config.scl_pullup = true;

        // SAFETY: This is the first and only time we're using these peripherals
        let mut i2c = unsafe {
            I2c::new_blocking(
                I2C2::steal(),
                PA12::steal(),
                PA11::steal(),
                Hertz(100_000),
                i2c_config,
            )
        };

        // Try to wake up SHT41 first
        info!("Attempting to wake up SHT41 @ 0x{:02X}...", SHT41_ADDR);
        let wake_result = i2c.blocking_write(SHT41_ADDR, &[CMD_MEASURE_HIGH_PRECISION]);
        match wake_result {
            Ok(_) => info!("✓ SHT41 wake command sent successfully"),
            Err(_) => info!("✗ SHT41 wake failed"),
        }

        Timer::after_millis(100).await;

        // Scan I2C bus - full scan to find all devices
        info!("Scanning I2C2 bus (full scan)...");
        let mut found_count = 0;
        for addr in 0x00..=0x7F {
            let mut buf = [0u8; 1];
            if i2c.blocking_read(addr, &mut buf).is_ok() {
                info!("✓ Device at 0x{:02X}", addr);
                found_count += 1;
            }
        }
        info!("Total devices found: {}", found_count);
    }

    // ============================================
    // Initialize LoRaWAN Radio Hardware
    // ============================================
    info!("Initializing LoRaWAN radio hardware...");

    // RF switch control pins (NUCLEO-WL55JC1 board)
    let ctrl1 = Output::new(p.PC4.degrade(), Level::Low, Speed::High);
    let ctrl2 = Output::new(p.PC5.degrade(), Level::Low, Speed::High);
    let ctrl3 = Output::new(p.PC3.degrade(), Level::High, Speed::High);
    info!("✓ RF switch pins configured (PC3, PC4, PC5)");

    // Initialize SubGHz SPI
    let spi = Spi::new_subghz(p.SUBGHZSPI, p.DMA1_CH1, p.DMA1_CH2);
    let spi = SubghzSpiDevice(spi);
    info!("✓ SubGHz SPI initialized");

    // Configure radio
    let use_high_power_pa = true; // Use high power PA for better range
    let config = sx126x::Config {
        chip: Stm32wl { use_high_power_pa },
        tcxo_ctrl: Some(TcxoCtrlVoltage::Ctrl1V7),
        use_dcdc: true,
        rx_boost: false,
    };

    // Create interface variant with RF switch control
    let iv = Stm32wlInterfaceVariant::new(
        Irqs,
        use_high_power_pa,
        Some(ctrl1),
        Some(ctrl2),
        Some(ctrl3),
    )
    .unwrap();
    info!("✓ RF switch interface variant created");

    // Initialize LoRa radio (this will be used for LoRaWAN later)
    let lora = LoRa::new(Sx126x::new(spi, iv, config), true, Delay)
        .await
        .unwrap();
    info!("✓ LoRa radio initialized");

    // Convert to LorawanRadio wrapper
    let radio: LorawanRadio<_, _, MAX_TX_POWER> = lora.into();

    // Configure AU915 region
    let mut au915 = AU915::new();
    au915.set_join_bias_and_noncompliant_retries(Subband::_1, 20); // Keep retrying on sub-band 1
    let region: region::Configuration = au915.into();
    info!("✓ AU915 region configured (sub-band 1)");

    // Initialize RNG for crypto
    let rng = Rng::new(p.RNG, Irqs);
    info!("✓ RNG initialized for crypto");

    // Create LoRaWAN device
    let mut device: Device<_, DefaultFactory, _, _> =
        Device::new(region, radio, EmbassyTimer::new(), rng);
    info!("✓ LoRaWAN device created");

    info!("========================================");
    info!("  LoRaWAN stack initialized!");
    info!("  Ready to join network");
    info!("========================================");

    // Initialize LED
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

    // Text style for display
    let text_style = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);

    // Sensor and display state
    let mut temp_int = 0i16;
    let mut hum_int = 0i16;
    let mut tx_count = 0u32;
    let mut uplink_counter = 0u32;
    let mut snr = 0i8;
    let mut rssi = 0i16;
    let mut dr: u8 = 0; // AU915 starts at DR0 = SF12
    const UPLINK_INTERVAL: u32 = 15;

    // LoRaWAN state
    let mut is_joined = false;
    let mut join_attempt = 0u32;

    // Uplink failure tracking: any failed send increments counter
    let mut uplink_fail_count = 0u8;
    const MAX_UPLINK_FAILS: u8 = 3;
    const CONFIRMED_UPLINK_INTERVAL: u32 = 5;  // Every 5th uplink is confirmed

    // Exponential backoff for join retries (ticks of ~2s each)
    // Starts at 8 ticks (~16s), doubles each failure, caps at 30 ticks (~60s)
    let mut join_backoff_ticks: u32 = 8;
    let mut join_wait_counter: u32 = 0;
    const JOIN_BACKOFF_MAX: u32 = 30;

    let join_mode = JoinMode::OTAA {
        deveui: DevEui::from(DEV_EUI),
        appeui: AppEui::from(APP_EUI),
        appkey: AppKey::from(APP_KEY),
    };

    info!("Starting OTAA join procedure...");
    info!("DevEUI: {:02X}", DEV_EUI);
    info!("AppEUI: {:02X}", APP_EUI);

    // Macro-like closure to read sensor and update display without flicker
    // Uses fixed-width formatting and overwrites previous content
    let mut read_sensor_and_update_display = |temp_int: &mut i16, hum_int: &mut i16, tx_count: u32, is_joined: bool, join_attempt: u32, snr: i8, rssi: i16, dr: u8| {
        let mut i2c_config = I2cConfig::default();
        i2c_config.sda_pullup = true;
        i2c_config.scl_pullup = true;

        let mut i2c = unsafe {
            I2c::new_blocking(
                I2C2::steal(),
                PA12::steal(),
                PA11::steal(),
                Hertz(100_000),
                i2c_config,
            )
        };

        // Read SHT41 sensor
        if i2c.blocking_write(SHT41_ADDR, &[CMD_MEASURE_HIGH_PRECISION]).is_ok() {
            // Small blocking delay for sensor (can't use async in closure)
            cortex_m::asm::delay(480_000); // ~10ms at 48MHz
            let mut data = [0u8; 6];
            if i2c.blocking_read(SHT41_ADDR, &mut data).is_ok() {
                let temp_raw = ((data[0] as u16) << 8) | (data[1] as u16);
                let hum_raw = ((data[3] as u16) << 8) | (data[4] as u16);
                *temp_int = -45 + ((175 * temp_raw as i32) / 65535) as i16;
                *hum_int = -6 + ((125 * hum_raw as i32) / 65535) as i16;
                info!("✓ SHT41: {}°C, {}% RH", *temp_int, *hum_int);
            }
        }

        // Update OLED display (SH1106 128x64)
        let mut display: GraphicsMode<_> = Builder::new()
            .with_size(DisplaySize::Display128x64)
            .connect_i2c(i2c)
            .into();

        // Clear buffer and redraw (buffer clear doesn't flicker - only flush sends to display)
        display.clear();

        // Line 1: Title + TX count or join status (fixed width)
        let mut line1 = heapless::String::<32>::new();
        if is_joined {
            let _ = core::fmt::write(&mut line1, format_args!("LoRa-1     Tx:{:>4}  ", tx_count));
        } else {
            let _ = core::fmt::write(&mut line1, format_args!("LoRa-1   Join:{:>2}    ", join_attempt));
        }
        let _ = Text::new(&line1, Point::new(0, 10), text_style).draw(&mut display);

        // Line 2: Temperature (fixed width)
        let mut line2 = heapless::String::<32>::new();
        let _ = core::fmt::write(&mut line2, format_args!("Temp: {:>3} C          ", *temp_int));
        let _ = Text::new(&line2, Point::new(0, 22), text_style).draw(&mut display);

        // Line 3: Humidity (fixed width)
        let mut line3 = heapless::String::<32>::new();
        let _ = core::fmt::write(&mut line3, format_args!("Hum:  {:>3} %          ", *hum_int));
        let _ = Text::new(&line3, Point::new(0, 34), text_style).draw(&mut display);

        // Line 4: RSSI + SNR (fixed width)
        let mut line4 = heapless::String::<32>::new();
        if is_joined {
            let _ = core::fmt::write(&mut line4, format_args!("RSSI:{:>4} SNR:{}{:>3}  ", rssi, if snr >= 0 { "+" } else { "" }, snr));
        } else {
            let _ = core::fmt::write(&mut line4, format_args!("                    "));
        }
        let _ = Text::new(&line4, Point::new(0, 46), text_style).draw(&mut display);

        // Line 5: DR/SF or join status
        let mut line5 = heapless::String::<32>::new();
        if !is_joined {
            let _ = core::fmt::write(&mut line5, format_args!("Connecting...       "));
        } else {
            let sf = 12u8.saturating_sub(dr);
            let _ = core::fmt::write(&mut line5, format_args!("DR{} SF{}            ", dr, sf));
        }
        let _ = Text::new(&line5, Point::new(0, 58), text_style).draw(&mut display);

        // Single flush sends complete frame to display (no flicker)
        let _ = display.flush();
    };

    // ============================================
    // Initial Join: first attempt before entering main loop
    // ============================================
    join_attempt += 1;
    info!("OTAA join attempt {} (backoff={}s)", join_attempt, join_backoff_ticks * 2);

    read_sensor_and_update_display(
        &mut temp_int, &mut hum_int, tx_count,
        is_joined, join_attempt, 0, 0, 0,
    );

    match device.join(&join_mode).await {
        Ok(JoinResponse::JoinSuccess) => {
            info!("✓ LoRaWAN joined on first attempt!");
            is_joined = true;
            join_backoff_ticks = 8;
        }
        Ok(JoinResponse::NoJoinAccept) => {
            error!("✗ Initial join failed: No join accept");
        }
        Err(err) => {
            error!("✗ Initial join error: {:?}", err);
        }
    }

    if is_joined {
        info!("========================================");
        info!("  LoRaWAN OTAA Join Complete!");
        info!("  Device is now connected");
        info!("========================================");
    }

    // ============================================
    // Main Loop: Sensor + Display + (optional) LoRaWAN
    // ============================================
    info!("Starting sensor + display loop...");

    loop {
        uplink_counter += 1;

        // Read sensor and update display
        let current_snr = if is_joined { snr } else { 0 };
        let current_rssi = if is_joined { rssi } else { 0 };

        read_sensor_and_update_display(
            &mut temp_int, &mut hum_int, tx_count,
            is_joined, join_attempt, current_snr, current_rssi, dr,
        );

        if is_joined {
            // ----------------------------------------
            // Send uplink at UPLINK_INTERVAL
            // ----------------------------------------
            if uplink_counter >= UPLINK_INTERVAL {
                uplink_counter = 0;

                let temp_encoded = (temp_int * 100) as i16;
                let hum_encoded = (hum_int * 100) as u16;

                let payload: [u8; 4] = [
                    (temp_encoded >> 8) as u8,
                    temp_encoded as u8,
                    (hum_encoded >> 8) as u8,
                    hum_encoded as u8,
                ];

                let use_confirmed = (tx_count % CONFIRMED_UPLINK_INTERVAL) == 0;

                if use_confirmed {
                    info!("Sending CONFIRMED uplink #{}: {}°C, {}%", tx_count, temp_int, hum_int);
                } else {
                    info!("Sending uplink #{}: {}°C, {}%", tx_count, temp_int, hum_int);
                }
                led.set_high();

                match device.send(&payload, 1, use_confirmed).await {
                    Ok(SendResponse::NoAck) => {
                        // Confirmed uplink sent but no ACK — gateway is unreachable.
                        uplink_fail_count += 1;
                        error!("✗ No ACK ({}/{})", uplink_fail_count, MAX_UPLINK_FAILS);
                        if uplink_fail_count >= MAX_UPLINK_FAILS {
                            error!("  Too many missed ACKs - rejoining");
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
                        // Only reset on a response that proves the gateway heard us
                        // (DownlinkReceived or RxComplete on a confirmed uplink).
                        // RxComplete on unconfirmed gives no gateway feedback, so leave
                        // the counter alone — it can only be cleared by a confirmed ACK.
                        if use_confirmed {
                            uplink_fail_count = 0;
                        }
                        info!("✓ Uplink sent: {:?} | DR{} SF{} SNR:{} RSSI:{}", response, dr, 12u8.saturating_sub(dr), snr, rssi);
                    }
                    Err(err) => {
                        error!("✗ Uplink radio error: {:?}", err);
                        if use_confirmed {
                            uplink_fail_count += 1;
                            if uplink_fail_count >= MAX_UPLINK_FAILS {
                                error!("  Too many uplink failures - rejoining");
                                is_joined = false;
                                join_attempt = 0;
                                uplink_fail_count = 0;
                                join_backoff_ticks = 8;
                                join_wait_counter = 0;
                            }
                        }
                    }
                }

                led.set_low();
                Timer::after_millis(100).await;
            }
        } else {
            // ----------------------------------------
            // Not joined: exponential backoff retry
            // ----------------------------------------
            join_wait_counter += 1;

            if join_wait_counter >= join_backoff_ticks {
                join_wait_counter = 0;
                join_attempt += 1;
                info!("OTAA join attempt {} (backoff was {}s)...", join_attempt, join_backoff_ticks * 2);

                match device.join(&join_mode).await {
                    Ok(JoinResponse::JoinSuccess) => {
                        info!("✓ LoRaWAN joined successfully!");
                        is_joined = true;
                        uplink_fail_count = 0;
                        join_backoff_ticks = 8;
                    }
                    Ok(JoinResponse::NoJoinAccept) => {
                        join_backoff_ticks = (join_backoff_ticks * 2).min(JOIN_BACKOFF_MAX);
                        error!("✗ Join failed - next retry in {}s", join_backoff_ticks * 2);
                    }
                    Err(err) => {
                        join_backoff_ticks = (join_backoff_ticks * 2).min(JOIN_BACKOFF_MAX);
                        error!("✗ Join error: {:?} - next retry in {}s", err, join_backoff_ticks * 2);
                    }
                }
            }
        }

        Timer::after_secs(2).await;
    }
}
