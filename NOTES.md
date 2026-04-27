# Notes

## Probe IDs (ST-LINK V3)

| Board  | Probe Serial                             | Flash Command                                        |
|--------|------------------------------------------|------------------------------------------------------|
| lora-1 | `0483:374e:003E00463234510A33353533`     | `cargo run --release` (from firmware/lora-1)         |
| lora-2 | `0483:374e:0026003A3234510A33353533`     | `cargo run --release` (from firmware/lora-2)         |

When both boards are connected, probe-rs will prompt for selection. Use:

```bash
probe-rs run --chip STM32WL55JCIx --probe 0483:374e:003E00463234510A33353533 target/thumbv7em-none-eabihf/release/lora-1
```

## Device EUIs

| Board  | DevEUI               |
|--------|----------------------|
| lora-1 | `23ce1bfeff091fac`   |
| lora-2 | `24ce1bfeff091fac`   |

## Gateway

- RAK7268V2 at `10.10.10.254`
- MQTT broker on port 1883
- AU915 sub-band 1

## MQTT Topics (RAK built-in server)

```text
application/TOT/device/23ce1bfeff091fac/rx   # lora-1 uplinks
application/TOT/device/24ce1bfeff091fac/rx   # lora-2 uplinks
```

## Join / Rejoin Algorithm

Both boards use the same algorithm (see `src/main.rs` main loop):

- **Initial join**: single attempt at startup before entering main loop
- **Exponential backoff**: starts at 16s (~8 ticks × 2s), doubles on each failure, caps at **60s** (30 ticks)
- **Backoff resets** to 16s on successful join
- **Gateway loss → rejoin**: 3 consecutive missed ACKs triggers `is_joined = false` and resets backoff
- **lora-1**: every 5th uplink is confirmed; failure counter only increments on missed ACKs, only resets on confirmed ACK
- **lora-2**: every uplink is confirmed (range probe — end-to-end ACK verification on every packet)
- `join_attempt` counter resets to 0 on every rejoin trigger so the OLED display stays meaningful

**Why 60s cap (not 10 min):** Field testing requires frequent power cycles. The old 10-min cap meant after 4–5 missed join attempts the board appeared stuck for up to 10 minutes. With a 60s cap, worst case is one missed attempt then back online within a minute.

**Sub-band bias:** Both boards call `set_join_bias_and_noncompliant_retries(Subband::_1, 20)` — the first 20 join attempts stay on sub-band 1 (matching the gateway config) before falling back to spec-compliant rotation across all sub-bands. `set_join_bias` (the old call) only biased the *first* attempt; any transient RF miss on that one attempt caused all retries to scatter across wrong sub-bands.

## lora-2 Sensor Change: BME688 → SHT41

lora-2 was originally fitted with a BME688 (pressure/humidity/gas) sensor. It was replaced with an SHT41 (temperature/humidity only) to match lora-1. Both boards are now identical hardware. Firmware differs: lora-2 sends all uplinks confirmed and transmits a TX counter rather than sensor readings (it serves as a range probe, not a sensor node).

**Why the BME688 caused problems:**

The BME688 shares I2C2 with the SH1106 OLED. On every power-up, the BME688 holds SDA low mid-transaction (internal boot sequence), which corrupts the I2C bus state before the display is even touched. The SH1106's `flush()` sends a large (~1KB) I2C burst which NACKs after ~32 bytes when the bus is dirty. Result: first 2 display lines OK, rest gibberish.

Attempted fixes (all failed):

- `bus_recover()` (toggle SCL 9×) before display writes — BME688 re-grabbed SDA between lines
- Sensor AFTER display — same NACK cascade from previous iteration's bus state
- One I2C instance per function call vs per transaction — made things worse
- Custom raw I2C driver (8-byte max writes) — worked in isolation (`display_test` binary) but failed in main loop once LoRaWAN and sensor traffic shared the bus

**Root cause:** `sh1106::flush()` sends the full 1KB framebuffer in one I2C transaction. This works fine when the bus is idle (lora-1, SHT41 only), but the BME688 periodically re-asserts SDA during its own internal state transitions, NACKing the flush mid-frame.

**Fix:** Replace BME688 with SHT41. SHT41 never holds SDA unexpectedly, bus stays clean, `flush()` works every time.

**Lesson:** When two I2C devices share a bus and one is pathologically poorly behaved (BME688 at power-up), the only reliable fix is hardware isolation (separate I2C bus) or removing the offending device.

## BME680 Pressure Calibration (lora-2)

Bosch full compensation requires reading calibration NVM coefficients from two register blocks (0x8A and 0xE1). Attempted this but got `press_raw=0x80000` (524288) — the BME680 "data not ready" sentinel — suggesting the compensation was not helping.

Reverted to a linear scaling approach:

```rust
let press_pa = ((press_raw * 195) / 1000) as u32;
*pressure_int = (press_pa / 100) as i16;
```

The multiplier 195 was back-calculated from: Brisbane actual ~1021 hPa vs old formula (×295) giving ~1546 hPa → correction factor 1021/1546 ≈ 0.661 → 295 × 0.661 ≈ 195. Good enough for relative readings; will drift slightly with weather.

## BME688 Gas Resistance — Known Limitation

The sensor on lora-2 is a **BME688** (chip ID `0x61`), not a BME680. The BME688 uses a different gas scanning architecture — the `run_gas_l` / `run_gas_h` bits and parallel mode gas scanning are not compatible with the simple BME680 forced-mode gas register approach.

Attempted:

- Proper Bosch `calc_res_heat` formula with calibration coefficients (par_gh1/2/3, res_heat_range, res_heat_val) — correctly computes `res_heat=0x67` for 320°C target at 28°C ambient
- Both `run_gas_l` (0x10) and `run_gas_h` (0x20) in CTRL_GAS_1

Result: `gas_valid=0, heat_stab=0` always — measurement skipped by hardware. Properly supporting BME688 gas requires implementing the full BME688 scanning mode API. Gas resistance will read 0 until then.

## lora-2 Transmission Rate Fix

lora-1 has an explicit `Timer::after_secs(2).await` at the end of each loop iteration. lora-2 originally relied on the BME680 blocking read (~2s) for loop timing — but if reads complete faster, the loop runs too fast. Fixed by adding the same `Timer::after_secs(2).await` to lora-2, making both boards consistent: sensor read every ~4s, uplink every ~60s (UPLINK_INTERVAL=15 × ~4s/tick).

## LoRaWAN — Spreading Factor, Data Rate, and ADR

### Spreading Factor (SF)

SF is the core LoRa modulation parameter. It controls how many chips (radio symbols) represent each bit:

| SF | Time-on-air (12 byte payload) | Approx. throughput | Link budget vs SF7 |
|----|-------------------------------|--------------------|--------------------|
| SF7  | ~50 ms  | ~5.5 kbps | baseline |
| SF8  | ~100 ms | ~3.1 kbps | +2.5 dB |
| SF9  | ~185 ms | ~1.8 kbps | +5 dB |
| SF10 | ~370 ms | ~980 bps  | +7.5 dB |
| SF11 | ~740 ms | ~440 bps  | +10 dB |
| SF12 | ~1500 ms| ~250 bps  | +12.5 dB |

Higher SF = better range, but longer airtime = more power, less capacity, and stricter duty cycle limits.

### Data Rate (DR) in AU915

DR is an index that bundles SF + bandwidth. Uplink channels (sub-band 1, 125 kHz):

| DR | SF | BW | Max payload |
|----|----|----|-------------|
| DR0 | SF12 | 125 kHz | 59 bytes |
| DR1 | SF11 | 125 kHz | 59 bytes |
| DR2 | SF10 | 125 kHz | 59 bytes |
| DR3 | SF9  | 125 kHz | 123 bytes |
| DR4 | SF8  | 125 kHz | 250 bytes |
| DR5 | SF7  | 125 kHz | 250 bytes |
| DR6 | SF8  | 500 kHz | 250 bytes (join only) |

Devices always join at DR0 (SF12) by default for maximum range.

### Adaptive Data Rate (ADR)

ADR is the mechanism by which the network server automatically moves a device to a higher DR (lower SF) when signal quality permits. The goal is minimum airtime and power use.

**How it works:**
1. Device sends uplinks with `ADR=1` bit set in the MAC header
2. Network server accumulates SNR/RSSI history (typically 20 frames)
3. When average SNR has enough margin above the minimum for the current DR, server sends a `LinkADRReq` MAC command in a downlink
4. `LinkADRReq` specifies new DR and TX power
5. Device acknowledges with `LinkADRAns` and switches

**ADR margin:** The server keeps a safety margin (typically 15 dB) so a brief fade doesn't immediately cause packet loss. ADR steps up aggressively, steps down conservatively.

**Why our boards stay at DR0 (SF12):**
- SNR of +4–+7 dB is strong, so ADR *should* step up
- Likely causes: ADR not enabled on the RAK gateway application, or the RAK built-in LoRa server has conservative ADR settings
- Check: RAK gateway UI → Application → Device → ADR enabled?

**What to watch on the OLED:** Once ADR kicks in, line 5 will change from `DR0 SF12` → `DR1 SF11` → ... → `DR5 SF7` over successive uplinks. Each step is a `LinkADRReq` downlink from the gateway.

### SNR and RSSI

**RSSI** (Received Signal Strength Indicator): total received power in dBm. More negative = weaker. -120 dBm is near the noise floor, -20 dBm is very strong.

**SNR** (Signal-to-Noise Ratio): signal power relative to noise floor, in dB. LoRa can decode below 0 dB SNR — this is unique to LoRa:

| SF | Minimum decodable SNR |
|----|-----------------------|
| SF7  | -7.5 dB |
| SF8  | -10 dB |
| SF9  | -12.5 dB |
| SF10 | -15 dB |
| SF11 | -17.5 dB |
| SF12 | -20 dB |

Our boards at +4–+7 dB SNR have ~24–27 dB of margin at SF12 — plenty for ADR to push to SF7.

**For field testing:** walk away from the gateway and watch RSSI drop and SNR fall. The ADR margin tells you how far you can go before packet loss. If SNR hits the minimum for the current SF, you'll see lost packets before the network server drops to a higher SF.

## Field Testing Methodology

### SNR vs DR — They Are Not the Same Thing

A common point of confusion when reading the lora-2 OLED during a walk test:

**DR0** is not a signal quality measurement. It is the modulation setting — SF12 + 125 kHz bandwidth. It tells you *how* the radio is transmitting, not how well the signal arrived. Think of it as the gear the radio is in. DR steps up (DR1, DR2...) only when ADR receives a `LinkADRReq` downlink from the gateway.

**SNR** is signal quality — how far above the noise floor the received signal sits. This is the number to watch during a range test.

They are unrelated. DR0 simply means ADR has not yet stepped up the modulation.

### Two Different SNR Values — LCD vs Grafana

You will notice the LCD and Grafana report different SNR values for the same board. This is not a bug — they are measuring two completely different radio paths:

| | LCD (e.g. +4 dB) | Grafana (e.g. +11 dB) |
|--|-----------------|----------------------|
| Measured by | Node receiver | Gateway receiver |
| Which signal | Gateway → Node (downlink ACK) | Node → Gateway (uplink) |
| Frequency | 923–928 MHz (RX1) | 915–928 MHz |
| Bandwidth | 500 kHz (AU915 RX1) | 125 kHz |

The gateway hears the node better than the node hears the gateway because:

- The gateway has a superior antenna, low-noise amplifier, and is typically mounted high
- AU915 RX1 downlinks use 500 kHz bandwidth — wider bandwidth means more noise, lower SNR at the node
- The node's receiver is a modest embedded radio, not a base-station-grade front end

Both readings are valid. Use the **LCD SNR for field testing** (it tells you what the node is experiencing). Use **Grafana SNR for post-analysis** (it tells you what the gateway received).

### Recording a Walk Test

You do not need to manually record RSSI, SNR, or DR during the test. InfluxDB timestamps every uplink to the second. All signal data is already there.

Your field notes only need **location and time**:

```text
09:14  Left gateway (0 m)
09:22  End of driveway (~200 m)
09:31  Front paddock gate (~500 m)
09:45  Creek crossing (~900 m)
09:58  OLED shows Connecting... — coverage edge
10:06  Signal resumed walking back
```

Back at the desk, open Grafana and overlay your timestamps against:

- **RSSI** — shows signal degradation over the walk
- **SNR** — shows when you approached the decoding limit
- **Frame count** — gaps reveal exactly which uplinks were lost and at what time

lora-2 sends a confirmed uplink every ~10s, so the Grafana timeline has enough resolution to match waypoint times precisely. The frame count gap tells you the exact moment and location of first packet loss.

### What the Numbers Mean in the Field

**Watch SNR, not just RSSI.** RSSI can read −100 dBm while SNR is still healthy. Conversely, SNR can collapse even when RSSI looks reasonable in a noisy RF environment.

At DR0 (SF12) the decoding limit is **−20 dB SNR**. In practice expect packet loss to begin around **−15 dB SNR** due to multipath and fading. When the LCD SNR approaches 0 dB and keeps falling, you are getting close to the edge.

**Coverage boundary** = where `Connecting...` first appears consistently on the OLED. Mark the time, walk back, and let the board rejoin (16–60s backoff). The TX counter will resume from where it left off — session keys survive a rejoin.

## Rust + Embassy on Bare-Metal STM32

### Why Rust

The STM32WL55 has no FPU, no OS, and tight memory constraints — the exact environment where C has historically ruled and where bugs are hardest to find. Rust's ownership and borrow checker catches at compile time the class of bugs that are silent disasters in C:

- **Buffer overflows** — impossible in safe Rust
- **Use-after-free / dangling pointers** — caught at compile time
- **Data races** — the type system enforces exclusive or shared access
- **Uninitialized memory** — variables must be initialized before use

The result: if it compiles, a large class of memory safety bugs is already eliminated. On hardware where a crash means a silent stuck node in the field, that matters.

### Why Embassy

Embassy is an async runtime for bare-metal embedded Rust. It replaces the traditional RTOS (FreeRTOS, Zephyr) with Rust's native `async/await` model:

- **No heap required** — async state machines are stack-allocated
- **No context switch overhead** — cooperative, not preemptive
- **Naturally fits this workload** — "wait for radio TX, wait for sensor measurement, wait for timer" maps perfectly onto `async/await`
- **Type-safe peripheral access** — the HAL enforces that you can't use a peripheral from two places simultaneously (moves and borrows at the type level)

The firmware pattern:

```rust
// Wait for LoRa TX to complete — suspends task, doesn't block CPU
device.send(&payload, 1, false).await;

// Wait for sensor measurement — same
Timer::after_millis(10).await;
```

Without Embassy (or an RTOS), you'd poll these manually with state machines and flags.

### The Trade-offs

**Harder entry point than C:**

- The borrow checker rejects patterns that are idiomatic in C (e.g. shared mutable state, self-referential structs)
- Embedded Rust's `no_std` environment has a smaller ecosystem than C — some drivers don't exist or are immature
- Async on embedded is still maturing — the `embassy-time` version pinning issue with `lora-phy` is a direct example

**Worth it because:**

- Bugs caught at compile time don't happen in the field
- The async model scales cleanly — adding more concurrent tasks doesn't require restructuring the whole firmware
- Rust's type system makes the hardware abstraction layer (embassy-stm32) genuinely safer — peripheral ownership is tracked, not assumed

### The Patched Crates Situation

`lora-phy` and `lorawan-device` published on crates.io depend on an older `embassy-time` API. Embassy moved to 0.4.0 and broke the interface. The fix was to fork both crates locally (`firmware/lora-phy-patched`, `firmware/lorawan-device-patched`) and update the `embassy-time` calls.

This is a known growing pain in the embedded Rust ecosystem — crate versions lag behind Embassy releases. The community is working on it (embassy now has more stable API guarantees), but for now: **do not `cargo update` without testing**.

### Target Triple

```text
thumbv7em-none-eabihf
```

- `thumbv7em` — ARM Cortex-M4 with Thumb2 instruction set
- `none` — no OS
- `eabihf` — hard-float ABI


Despite the `hf` suffix, the STM32WL55 **has no FPU**. The `eabihf` ABI is used anyway because the LoRa PHY crate expects it. All floating-point operations must use integer math with fixed-point scaling (e.g. temperature × 100 stored as `i16`).

## Gateway Loss Detection — SendResponse::NoAck

### The problem

When a gateway is powered off, LoRaWAN nodes have no passive notification mechanism. The radio is fire-and-forget on uplinks. Initially the firmware tried to detect gateway loss by counting consecutive uplink failures with `uplink_fail_count`, but the counter never reached the threshold and the OLED stayed on "Connected" indefinitely.

**Why the original logic was broken:**

The failure counter was reset inside the `Ok(response)` match arm. For unconfirmed uplinks, `lorawan-device` returns `Ok(SendResponse::RxComplete)` regardless of whether the gateway received the packet — there is no ACK to wait for. With the uplink pattern at the time (1 confirmed every 5), the 4 unconfirmed uplinks between confirmed ones always returned `Ok` and reset `uplink_fail_count` to 0. The counter could never accumulate to 3.

### The fix

`lorawan-device` returns `Ok(SendResponse::NoAck)` — not `Err(...)` — when a confirmed uplink times out with no ACK in RX1 or RX2. `Err` is only returned for radio hardware faults.

The `SendResponse` enum:

```rust
pub enum SendResponse {
    DownlinkReceived(mac::FcntDown),
    SessionExpired,
    NoAck,        // confirmed uplink sent, no ACK received — gateway unreachable
    RxComplete,   // unconfirmed uplink complete — gives no gateway feedback
}
```

The fix matches on `NoAck` explicitly:

```rust
match device.send(&payload, 1, use_confirmed).await {
    Ok(SendResponse::NoAck) => {
        uplink_fail_count += 1;  // gateway is not responding
        // trigger rejoin after MAX_UPLINK_FAILS
    }
    Ok(response) => {
        // confirmed ACK received (DownlinkReceived or RxComplete on confirmed)
        if use_confirmed { uplink_fail_count = 0; }
    }
    Err(e) => { /* radio hardware fault */ }
}
```

**lora-2** sends every uplink confirmed so gateway loss is detected within 3 uplink cycles (~30s).

**lora-1** sends every 5th uplink confirmed. The failure counter only increments on `NoAck` from a confirmed send, and only resets when a confirmed send gets a real ACK. Unconfirmed `RxComplete` responses are neutral — they neither increment nor reset the counter. Detection time: up to 3 confirmed cycles × 5 × 30s = ~7.5 min worst case.

### Key lesson

In LoRaWAN, "uplink sent without error" and "gateway received uplink" are not the same thing. Only a confirmed uplink with an ACK downlink proves end-to-end connectivity. Unconfirmed uplinks are inherently best-effort and provide no liveness information about the network.

---

## Probe Busy (os error 16)

If `probe-rs run` fails with `Device or resource busy (os error 16)`, a previous session is still attached:

```bash
pkill -f "probe-rs"
```

Then retry the flash command.
