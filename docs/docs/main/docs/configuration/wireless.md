# Wireless and Bluetooth configuration

Enable Bluetooth, then configure the battery inputs and pairing options your keyboard needs. Add the examples below to your existing `keyboard.toml`; they are configuration fragments, not complete keyboard files.

## Before you begin

- Enable the Cargo feature for your BLE chip. See [supported wireless chips](../features/wireless#supported-microcontrollers).
- Use an nRF52 BLE board for the automatic battery and charger setup described below. Split peripherals must use a BLE split connection.
- Choose unused pins that match your ADC divider, charger status output, and LED wiring.

## Enable Bluetooth

Add `enabled = true` to `[ble]`:

```toml title="keyboard.toml"
[ble]
enabled = true
```

For nRF52, you can also set transmit power and enable 2M PHY:

```toml title="keyboard.toml"
[ble]
enabled = true
default_tx_power = 0
use_2m_phy = true
```

`default_tx_power` is in dBm; its supported range depends on the chip. These radio settings apply to nRF52 boards.

For a host adapter that cannot connect at 2M PHY, enable RMK's `use_1m_phy` Cargo feature. It selects 1M PHY for host connections. Split and dongle links continue to use 2M PHY.

## Measure the battery percentage

Choose the ADC setup that matches your board's wiring.

### Use a GPIO ADC input

Set the ADC pin and the divider values in `[ble]`. Both divider values must be greater than zero:

```toml title="keyboard.toml"
[ble]
enabled = true
battery_adc_pin = "P0_05"
adc_divider_measured = 2000
adc_divider_total = 2806
```

`adc_divider_measured` is the resistance between the ADC input and ground. `adc_divider_total` is the sum of both divider resistors. Use the same unit for both values.

The example uses a 2 MΩ lower resistor and an 806 kΩ upper resistor, expressed in kΩ. The battery voltage is the measured voltage multiplied by `2806 / 2000`. Replace the pin and values with those from your board's schematic.

### Use the internal VDDH input

On an nRF52 board powered through VDDH, use the internal input:

```toml title="keyboard.toml"
[ble]
enabled = true
battery_adc_pin = "vddh"
```

RMK uses VDDH's fixed 1:5 divider. TOML divider values are ignored for this input. Use a GPIO ADC input when VDDH does not reflect the battery voltage.

## Add a charging indicator

Add `charge_state` to read the charger's status output. Set `low_active = true` when a low pin level means charging.

Add `charge_led` to control an LED. Its polarity is independent of the status pin:

```toml title="keyboard.toml"
[ble]
enabled = true
battery_adc_pin = "P0_05"
adc_divider_measured = 2000
adc_divider_total = 2806
charge_state = { pin = "P0_20", low_active = true }
charge_led = { pin = "P0_21", low_active = false }
```

The LED lights while charging. When not charging, it blinks if the measured percentage is below 10%; otherwise it stays off. `charge_led` requires an ADC or charging-state input on the same board.

### Charging indicators without ADC

Omit the ADC settings when you only need the charging indicator:

```toml title="keyboard.toml"
[ble]
enabled = true
charge_state = { pin = "P0_20", low_active = true }
charge_led = { pin = "P0_21", low_active = false }
```

The LED lights while charging. This setup has no measured battery percentage or low-battery blinking. Configure an ADC input if you need to report a percentage to the host.

### Read battery status on the OLED

The default OLED battery label distinguishes the charging state from the measured percentage:

| Battery state                    | Label  |
| -------------------------------- | ------ |
| Measured at 42%, charging        | `42%+` |
| Measured at 42%, not charging    | `42%`  |
| Percentage unknown, charging     | `CHG`  |
| Percentage unknown, not charging | `?`    |
| No battery status available      | `N/A`  |

Unknown percentages have no filled battery bars. A percentage can be unknown before the first ADC sample, or after charging ends while the keyboard waits for its next sample. A charging-only setup keeps the percentage unknown.

## Split battery configuration

Configure each board's battery hardware in its existing split section:

```toml title="keyboard.toml"
[split.central]
battery_adc_pin = "P0_01"
adc_divider_measured = 2000
adc_divider_total = 2806
charge_state = { pin = "P0_20", low_active = true }
battery_user_description = "Left"

[[split.peripheral]]
battery_adc_pin = "P0_02"
adc_divider_measured = 2000
adc_divider_total = 2806
charge_state = { pin = "P0_20", low_active = true }
charge_led = { pin = "P0_21", low_active = false }
battery_user_description = "Right"
```

Merge the peripheral fields into the entry for that board; do not add a new peripheral just to configure its battery. Pins on different boards are independent.

| Board      | Effective settings                                                                                                                                                                                                    |
| ---------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Unibody    | Use `[ble]`.                                                                                                                                                                                                          |
| Central    | A `battery_adc_pin` in `[split.central]` replaces the entire `[ble]` ADC group. Omitted divider values in that group default to `1`. Each omitted `charge_state` or `charge_led` falls back to `[ble]` independently. |
| Peripheral | Use only that board's `[[split.peripheral]]` settings. It does not inherit battery hardware from `[ble]`.                                                                                                             |

A `"vddh"` input always uses the fixed 1:5 divider, including on split boards.

## Using the Rust API

TOML projects construct and run the battery components automatically. In Rust API projects, initialize the GPIOs with your board's HAL, then add the components to the existing task list.

The following fragment assumes `status_pin` is an initialized input, `led_pin` is an initialized output, and `keyboard` and `ble_transport` are your firmware's existing instances. The status pin is active low; the LED is active high:

```rust
use rmk::input_device::battery::{BatteryProcessor, ChargingStateReader};
use rmk::processor::builtin::battery_led::BatteryLedProcessor;
use rmk::run_all;

let mut charging = ChargingStateReader::new(status_pin, true);
let mut battery = BatteryProcessor::charging_only();
let mut led = BatteryLedProcessor::new(led_pin, false);
run_all!(charging, battery, led, keyboard, ble_transport).await;
```

For percentage measurement, replace `charging_only()` with `BatteryProcessor::new(measured, total)` and run your ADC input alongside it. Use `BatteryProcessor::new(1, 5)` for an internal VDDH input. The processor uses the divider you supply. The nRF ADC task must publish `BatteryAdcEvent`; the [nRF52840 Rust example](https://github.com/rmk-rs/rmk/blob/main/examples/use_rust/nrf52840_ble/src/main.rs) shows its initialization. Run one `BatteryProcessor` per board, whether you have ADC input, charging input, or both.

`BleBatteryConfig` controls central battery notifications. Before constructing `BleTransport`, set it on your existing `rmk_config` instance:

```rust
use rmk::config::BleBatteryConfig;

rmk_config.ble_battery_config = BleBatteryConfig { enabled: true };
```

Use `BleBatteryConfig::disabled()` to disable central notifications. This option does not configure GPIOs or start battery components. For changes to an existing Rust project, see [battery setup migration](../migration/v09_v10#battery-setup).

## Name batteries reported to the host

BLE split keyboards expose one Battery Service for the central and one for each peripheral with `battery_adc_pin`. Configure the display names in `battery_user_description`:

- Use `[ble]` for a unibody or central default name.
- Use `[split.central]` to override the central name.
- Use `[[split.peripheral]]` to name that peripheral. A peripheral name requires its `battery_adc_pin` to be configured.

The default names are `Central`, `Peripheral 0`, `Peripheral 1`, and so on. A host must support discovering multiple Battery Service instances to show each battery separately.

### Adjust the client attribute table

If your enabled BLE services need more client-specific attribute space, such as CCCDs, set `TROUBLE_HOST_CLIENT_ATT_TABLE_SIZE` in `.cargo/config.toml`:

```toml title=".cargo/config.toml"
[env]
TROUBLE_HOST_CLIENT_ATT_TABLE_SIZE = "128"
```

This project-wide setting overrides trouble-host's Cargo feature settings. Choose the size required by your enabled services.

## Passkey entry

Enable RMK's `passkey_entry` Cargo feature, then add the pairing settings to `[ble]`:

```toml title="keyboard.toml"
[ble]
enabled = true
passkey_entry = true
passkey_entry_timeout = 120
```

Passkey entry is disabled by default. The timeout defaults to 120 seconds and must be at least 30 seconds. During entry, the keyboard sends no normal keypresses to the host:

| Key                         | Action                        |
| --------------------------- | ----------------------------- |
| `0`–`9` (top row or numpad) | Enter a digit.                |
| `Enter` or `Numpad Enter`   | Submit the six-digit passkey. |
| `Escape`                    | Cancel pairing.               |
| `Backspace`                 | Delete the last digit.        |

Other keys are ignored. If the timeout expires, pairing is cancelled. See [BLE passkey entry](../features/wireless#ble-passkey-entry) for the pairing workflow.
