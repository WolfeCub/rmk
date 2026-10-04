use core::cell::Cell;

use embassy_sync::blocking_mutex::Mutex;
use embedded_hal::digital::InputPin;
use rmk_macro::{input_device, processor};
use rmk_types::battery::{BatteryStatus, ChargeState};

use crate::RawMutex;
use crate::event::{BatteryAdcEvent, BatteryStatusEvent, ChargingStateEvent, publish_event};

/// Cached battery status, updated by [`BatteryProcessor::commit`] alongside every
/// [`BatteryStatusEvent`] publish so host services can read the current value
/// synchronously without subscribing to the event stream.
pub(crate) static BATTERY_STATUS: Mutex<RawMutex, Cell<BatteryStatus>> =
    Mutex::new(Cell::new(BatteryStatus::Unavailable));

pub(crate) fn current_battery_status() -> BatteryStatus {
    BATTERY_STATUS.lock(|c| c.get())
}

/// Publishes the initial GPIO charging state and subsequent changes.
#[input_device(publish = ChargingStateEvent)]
pub struct ChargingStateReader<I: InputPin> {
    // Charging state pin or standby pin
    state_input: I,
    // True: low represents charging, False: high represents charging
    low_active: bool,
    current_charging_state: Option<bool>,
}

impl<I: InputPin> ChargingStateReader<I> {
    pub fn new(state_input: I, low_active: bool) -> Self {
        Self {
            state_input,
            low_active,
            current_charging_state: None,
        }
    }

    /// Waits for the first reading or a change in charging state.
    async fn read_charging_state_event(&mut self) -> ChargingStateEvent {
        loop {
            let delay = if self.current_charging_state.is_none() { 2 } else { 5 };
            embassy_time::Timer::after_secs(delay).await;

            let charging_state = if self.low_active {
                self.state_input.is_low().unwrap_or(false)
            } else {
                self.state_input.is_high().unwrap_or(false)
            };

            if self.current_charging_state != Some(charging_state) {
                self.current_charging_state = Some(charging_state);
                return ChargingStateEvent {
                    charging: charging_state,
                };
            }
        }
    }
}

/// BatteryProcessor processes battery adc value and charging state,
/// emits `BatteryStatusEvent` when battery status changes.
#[processor(subscribe = [BatteryAdcEvent, ChargingStateEvent])]
pub struct BatteryProcessor {
    adc_divider: Option<(u32, u32)>,
    /// Current battery status
    battery_status: BatteryStatus,
}

impl BatteryProcessor {
    pub fn new(adc_divider_measured: u32, adc_divider_total: u32) -> Self {
        BatteryProcessor {
            adc_divider: Some((adc_divider_measured, adc_divider_total)),
            battery_status: BatteryStatus::Unavailable,
        }
    }

    /// Reports charger state without measuring a battery percentage.
    /// Run a `ChargingStateReader` alongside this processor.
    pub fn charging_only() -> Self {
        Self {
            adc_divider: None,
            battery_status: BatteryStatus::Unavailable,
        }
    }

    /// Apply a new battery status: persist on the processor, mirror into
    /// [`BATTERY_STATUS`] for synchronous readers, and broadcast via
    /// [`BatteryStatusEvent`].
    fn commit(&mut self, status: BatteryStatus) {
        self.battery_status = status;
        BATTERY_STATUS.lock(|c| c.set(status));
        publish_event(BatteryStatusEvent::from(status));
    }

    fn get_battery_percent(&self, val: u16) -> Option<u8> {
        let (measured, total) = self.adc_divider?;
        if measured == 0 || total == 0 {
            error!("Battery ADC divider values must be greater than zero");
            return Some(0);
        }
        // Undo the divider, then map the nRF SAADC range 4055..4755 to 0..100%.
        let normalized = i64::from(val) * i64::from(total) / i64::from(measured);
        Some(((normalized - 4055) / 7).clamp(0, 100) as u8)
    }

    async fn on_battery_adc_event(&mut self, event: BatteryAdcEvent) {
        let val = event.0;
        trace!("Detected battery ADC value: {:?}", val);

        let Some(battery_percent) = self.get_battery_percent(val) else {
            return;
        };
        match self.battery_status {
            // Skip ADC updates while charging
            BatteryStatus::Available {
                charge_state: ChargeState::Charging,
                ..
            } => {}
            // Not charging: publish if the percentage changed.
            BatteryStatus::Available { charge_state, level } => {
                if level != Some(battery_percent) {
                    self.commit(BatteryStatus::Available {
                        charge_state,
                        level: Some(battery_percent),
                    });
                }
            }
            // First ADC reading: transition from Unavailable.
            BatteryStatus::Unavailable => {
                self.commit(BatteryStatus::Available {
                    charge_state: ChargeState::Unknown,
                    level: Some(battery_percent),
                });
            }
        }
    }

    async fn on_charging_state_event(&mut self, event: ChargingStateEvent) {
        let charging = event.charging;
        info!("Charging state changed: {:?}", charging);

        let level = match self.battery_status {
            // ADC updates pause during charging, so refresh the level when charging ends.
            BatteryStatus::Available {
                charge_state: ChargeState::Charging,
                ..
            } if !charging => None,
            BatteryStatus::Available { level, .. } => level,
            BatteryStatus::Unavailable => None,
        };

        self.commit(BatteryStatus::Available {
            charge_state: charging.into(),
            level,
        });
    }
}

#[cfg(test)]
mod tests {
    use core::pin::pin;

    use embassy_futures::join::join3;
    use embassy_time::{Duration, MockDriver};
    use embedded_hal_mock::eh1::digital::{Mock, State, Transaction};
    use futures::poll;
    use rmk_types::battery::{BatteryStatus, ChargeState};

    use super::{BatteryProcessor, ChargingStateReader, current_battery_status};
    use crate::core_traits::Runnable;
    use crate::event::{BatteryAdcEvent, ChargingStateEvent};
    use crate::processor::builtin::battery_led::BatteryLedProcessor;
    use crate::test_support::test_block_on;

    #[test]
    fn zero_adc_divider_does_not_panic() {
        assert_eq!(BatteryProcessor::new(0, 1).get_battery_percent(2000), Some(0));
    }

    #[test]
    fn divider_rounding_boundary_does_not_underflow() {
        assert_eq!(BatteryProcessor::new(2000, 2806).get_battery_percent(2890), Some(0));
    }

    #[test]
    fn initial_discharging_state_is_reported() {
        let mut input = Mock::new(&[Transaction::get(State::High)]);
        let mut charging = ChargingStateReader::new(input.clone(), true);
        let event = test_block_on(charging.read_charging_state_event());
        assert!(!event.charging);
        input.done();
    }

    #[test]
    fn adc_readings_use_the_configured_divider_including_the_vddh_range() {
        test_block_on(async {
            let mut battery = BatteryProcessor::new(1, 6);
            battery.on_battery_adc_event(BatteryAdcEvent(750)).await;
            assert_eq!(
                current_battery_status(),
                BatteryStatus::Available {
                    charge_state: ChargeState::Unknown,
                    level: Some(63),
                }
            );
        });
    }

    #[test]
    fn charging_state_keeps_sampled_level_until_charging_ends() {
        test_block_on(async {
            let mut battery = BatteryProcessor::new(2000, 2806);
            battery.on_battery_adc_event(BatteryAdcEvent(2920)).await;

            // The first GPIO read refines Unknown to Discharging; it must not erase the ADC result.
            for charging in [false, true] {
                battery.on_charging_state_event(ChargingStateEvent { charging }).await;
                assert_eq!(
                    current_battery_status(),
                    BatteryStatus::Available {
                        charge_state: charging.into(),
                        level: Some(5),
                    }
                );
            }

            battery
                .on_charging_state_event(ChargingStateEvent { charging: false })
                .await;
            assert_eq!(
                current_battery_status(),
                BatteryStatus::Available {
                    charge_state: ChargeState::Discharging,
                    level: None,
                }
            );

            battery.on_battery_adc_event(BatteryAdcEvent(3200)).await;
            assert_eq!(
                current_battery_status(),
                BatteryStatus::Available {
                    charge_state: ChargeState::Discharging,
                    level: Some(62),
                }
            );
        });
    }

    #[test]
    fn charging_only_pipeline_drives_led_without_inventing_a_battery_level() {
        test_block_on(async {
            let mut input = Mock::new(&[Transaction::get(State::Low), Transaction::get(State::High)]);
            let mut output = Mock::new(&[
                Transaction::set(State::Low),
                // The LED's ready tick runs before its state event; it updates on the next tick.
                Transaction::set(State::Low),
                Transaction::set(State::High),
                Transaction::set(State::High),
                Transaction::set(State::High),
                Transaction::set(State::High),
                Transaction::set(State::High),
                Transaction::set(State::Low),
            ]);
            let mut charging = ChargingStateReader::new(input.clone(), true);
            let mut battery = BatteryProcessor::charging_only();
            let mut led = BatteryLedProcessor::new(output.clone(), false);
            let mut run = pin!(join3(charging.run(), battery.run(), led.run()));
            assert!(poll!(run.as_mut()).is_pending());

            for second in 1..=8 {
                MockDriver::get().advance(Duration::from_secs(1));
                assert!(poll!(run.as_mut()).is_pending());
                if matches!(second, 2 | 7) {
                    assert_eq!(
                        current_battery_status(),
                        BatteryStatus::Available {
                            charge_state: if second < 7 {
                                ChargeState::Charging
                            } else {
                                ChargeState::Discharging
                            },
                            level: None,
                        }
                    );
                }
            }
            input.done();
            output.done();
        });
    }
}
