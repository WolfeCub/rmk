use core::cell::{Cell, RefCell};
use core::convert::Infallible;
use core::future::Future;
use core::pin::{Pin, pin};
use core::task::{Context, Waker};
use std::vec::Vec;

use bt_hci::cmd::{self, AsyncCmd, Cmd, CmdReturnBuf, SyncCmd};
use bt_hci::controller::{Controller, ControllerCmdAsync, ControllerCmdSync};
use bt_hci::{ControllerToHostPacket, FromHciBytes};

use super::*;
use crate::core_traits::Runnable;
use crate::event::{BatteryAdcEvent, ChargingStateEvent, publish_event};
use crate::input_device::battery::BatteryProcessor;

struct ReadController<'a> {
    resume: &'a Cell<bool>,
    next: Cell<usize>,
    encrypted: bool,
    request: &'a RefCell<Option<Vec<u8>>>,
    response: &'a RefCell<Option<Vec<u8>>>,
}

impl embedded_io_async::ErrorType for ReadController<'_> {
    type Error = Infallible;
}

impl Controller for ReadController<'_> {
    type Buffer<'a> = [u8; 64];

    fn alloc_buf(&self) -> Result<Self::Buffer<'_>, Self::Error> {
        Ok([0; 64])
    }

    async fn write_acl_data(&self, packet: &bt_hci::data::AclPacket<'_>) -> Result<(), Self::Error> {
        assert_eq!(&packet.data()[2..4], &[4, 0]);
        assert!(
            self.response
                .borrow_mut()
                .replace(packet.data()[4..].to_vec())
                .is_none()
        );
        Ok(())
    }

    async fn write_sync_data(&self, _: &bt_hci::data::SyncPacket<'_>) -> Result<(), Self::Error> {
        panic!("unexpected synchronous write")
    }

    async fn write_iso_data(&self, _: &bt_hci::data::IsoPacket<'_>) -> Result<(), Self::Error> {
        panic!("unexpected ISO write")
    }

    async fn read<'a>(&self, buf: &'a mut Self::Buffer<'_>) -> Result<ControllerToHostPacket<'a>, Self::Error> {
        // HCI LE Connection Complete, then Encryption Change for connection handle 1.
        let events: [&[u8]; 2] = [
            &[4, 0x3e, 19, 1, 0, 1, 0, 1, 0, 1, 2, 3, 4, 5, 6, 24, 0, 0, 0, 0xf4, 1, 0],
            &[4, 8, 4, 0, 1, 0, 1],
        ];
        let next = self.next.get();
        if next >= if self.encrypted { 2 } else { 1 } {
            let request = core::future::poll_fn(|_| match self.request.borrow_mut().take() {
                Some(request) => core::task::Poll::Ready(request),
                None => core::task::Poll::Pending,
            })
            .await;
            buf[..request.len()].copy_from_slice(&request);
            return Ok(ControllerToHostPacket::from_hci_bytes_complete(&buf[..request.len()]).unwrap());
        }
        if next == 1 {
            core::future::poll_fn(|_| {
                if self.resume.get() {
                    core::task::Poll::Ready(())
                } else {
                    core::task::Poll::Pending
                }
            })
            .await;
        }
        self.next.set(next + 1);
        let event = events[next];
        buf[..event.len()].copy_from_slice(event);
        Ok(ControllerToHostPacket::from_hci_bytes_complete(&buf[..event.len()]).unwrap())
    }
}

impl<C: SyncCmd> ControllerCmdSync<C> for ReadController<'_> {
    async fn exec(&self, _: &C) -> Result<C::Return, cmd::Error<Self::Error>> {
        let mut buffer = C::ReturnBuf::new();
        if C::OPCODE == cmd::le::LeReadBufferSize::OPCODE {
            buffer.as_mut().copy_from_slice(&[64, 0, 32]);
        }
        Ok(C::Return::from_hci_bytes_complete(buffer.as_ref()).unwrap())
    }
}

impl<C: AsyncCmd> ControllerCmdAsync<C> for ReadController<'_> {
    async fn exec(&self, _: &C) -> Result<(), cmd::Error<Self::Error>> {
        panic!("unexpected HCI command")
    }
}

fn assert_pending(future: Pin<&mut impl Future>) {
    assert!(future.poll(&mut Context::from_waker(Waker::noop())).is_pending());
}

fn with_connection(encrypted: bool, test: impl FnOnce(&Server<'_>, &mut dyn FnMut(&[u8]) -> Vec<u8>)) {
    let mut resources: trouble_host::HostResources<DefaultPacketPool, 1, 1> = trouble_host::HostResources::new();
    let resume = Cell::new(false);
    let request = RefCell::new(None);
    let response = RefCell::new(None);
    let stack = trouble_host::new(
        ReadController {
            next: Cell::new(0),
            encrypted,
            resume: &resume,
            request: &request,
            response: &response,
        },
        &mut resources,
    )
    .build();
    stack
        .add_bond_information(BondInformation::new(
            trouble_host::Identity {
                addr: trouble_host::Address::new(
                    bt_hci::param::AddrKind::PUBLIC,
                    bt_hci::param::BdAddr::new([1, 2, 3, 4, 5, 6]),
                ),
                irk: None,
            },
            trouble_host::LongTermKey(42),
            SecurityLevel::Encrypted,
            true,
        ))
        .unwrap();
    let (mut rx, mut control, mut tx) = stack.runner().split();
    let mut control = pin!(control.run());
    assert_pending(control.as_mut());
    let mut rx = pin!(rx.run());
    assert_pending(rx.as_mut());
    let conn = stack.peripheral().try_accept().unwrap();
    resume.set(true);
    assert_pending(rx.as_mut());
    assert_eq!(conn.security_level().unwrap().encrypted(), encrypted);
    let server = Server::new(AttributeTable::new());
    let conn = conn.with_attribute_server(&server.server).unwrap();
    let mut gatt = pin!(super::super::gatt_events_task(&server, &conn));
    let mut tx = pin!(tx.run());
    test(&server, &mut |att| {
        let mut packet = std::vec![2, 1, 0x20];
        packet.extend_from_slice(&((att.len() + 4) as u16).to_le_bytes());
        packet.extend_from_slice(&(att.len() as u16).to_le_bytes());
        packet.extend_from_slice(&[4, 0]);
        packet.extend_from_slice(att);
        *request.borrow_mut() = Some(packet);
        assert_pending(rx.as_mut());
        assert_pending(gatt.as_mut());
        assert_pending(tx.as_mut());
        response.borrow_mut().take().expect("GATT event must send an ATT reply")
    });
}

#[test]
fn battery_reads_return_measurements_and_delegate_other_requests() {
    with_connection(true, |server, reply| {
        let [lo, hi] = server.battery_service.level.handle.to_le_bytes();
        assert_eq!(reply(&[0x0a, lo, hi]), [1, 0x0a, lo, hi, 0x0e]);
        assert_eq!(reply(&[0x0c, lo, hi, 0, 0]), [1, 0x0c, lo, hi, 0x0e]);
        let mut battery = BatteryProcessor::new(1, 1);
        let mut run = pin!(battery.run());
        assert_pending(run.as_mut());
        for (sample, level) in [(4055, 0), (4566, 73)] {
            publish_event(BatteryAdcEvent(sample));
            assert_pending(run.as_mut());
            assert_eq!(reply(&[0x0a, lo, hi]), [0x0b, level]);
            assert_eq!(reply(&[0x0c, lo, hi, 0, 0]), [0x0d, level]);
        }
        for charging in [true, false] {
            publish_event(ChargingStateEvent { charging });
            assert_pending(run.as_mut());
        }
        assert_eq!(reply(&[0x0a, lo, hi]), [0x0b, 73]);

        server.set(&server.battery_service.level, &42).unwrap();
        let [dl, dh] = server.device_config_service.manufacturer_name.handle.to_le_bytes();
        assert_eq!(reply(&[0x0a, dl, dh])[0], 0x0b);
        assert_eq!(reply(&[0x10, 1, 0, 0xff, 0xff, 0, 0x28])[0], 0x11);
        assert_eq!(server.get(&server.battery_service.level).unwrap(), 42);
    });
}

#[test]
fn battery_reads_keep_encryption_permissions() {
    with_connection(false, |server, reply| {
        let [lo, hi] = server.battery_service.level.handle.to_le_bytes();
        assert_eq!(reply(&[0x0a, lo, hi]), [1, 0x0a, lo, hi, 5]);
        let [dl, dh] = server.device_config_service.manufacturer_name.handle.to_le_bytes();
        assert_eq!(reply(&[0x20, dl, dh, lo, hi]), [1, 0x20, 0, 0, 5]);
        #[cfg(feature = "split")]
        {
            let [lo, hi] = server
                .peripheral_battery_services
                .levels
                .first()
                .expect("set KEYBOARD_TOML_PATH to tests/ble_battery.toml for split BLE tests")
                .handle
                .to_le_bytes();
            assert_eq!(reply(&[0x0a, lo, hi]), [1, 0x0a, lo, hi, 5]);
        }
    });
}

#[cfg(feature = "split")]
#[test]
fn peripheral_reads_use_their_own_history() {
    with_connection(true, |server, reply| {
        let peripheral = server
            .peripheral_battery_services
            .levels
            .first()
            .expect("set KEYBOARD_TOML_PATH to tests/ble_battery.toml for split BLE tests");
        let id = *crate::SPLIT_BATTERY_PERIPHERAL_IDS.first().unwrap();
        let [lo, hi] = peripheral.handle.to_le_bytes();
        assert_eq!(reply(&[0x0a, lo, hi]), [1, 0x0a, lo, hi, 0x0e]);
        let mut battery = BatteryProcessor::new(1, 1);
        let mut run = pin!(battery.run());
        assert_pending(run.as_mut());
        publish_event(BatteryAdcEvent(4405));
        assert_pending(run.as_mut());
        server.set(&server.battery_service.level, &42).unwrap();
        for (level, expected) in [(Some(0), 0), (Some(81), 81), (None, 81)] {
            crate::split::driver::set_peripheral_battery(
                id,
                rmk_types::battery::BatteryStatus::Available {
                    charge_state: rmk_types::battery::ChargeState::Discharging,
                    level,
                },
            );
            assert_eq!(reply(&[0x0a, lo, hi]), [0x0b, expected]);
            assert_eq!(reply(&[0x0c, lo, hi, 0, 0]), [0x0d, expected]);
        }
        assert_eq!(server.get(&server.battery_service.level).unwrap(), 42);
    });
}

#[test]
fn unsupported_batch_reads_keep_native_validation() {
    with_connection(true, |server, reply| {
        let [lo, hi] = server.battery_service.level.handle.to_le_bytes();
        let [dl, dh] = server.device_config_service.manufacturer_name.handle.to_le_bytes();
        assert_eq!(reply(&[0x20, lo, hi, 0xff, 0xff]), [1, 0x20, 0, 0, 1]);
        assert_eq!(reply(&[0x20, dl, dh, lo, hi]), [1, 0x20, lo, hi, 0x0e]);
        let mut battery = BatteryProcessor::new(1, 1);
        let mut run = pin!(battery.run());
        assert_pending(run.as_mut());
        publish_event(BatteryAdcEvent(4055));
        assert_pending(run.as_mut());
        server.set(&server.battery_service.level, &42).unwrap();
        assert_eq!(reply(&[0x20, dl, dh, lo, hi]), [1, 0x20, 0, 0, 6]);
        assert_eq!(reply(&[0x20, lo, hi]), [1, 0x20, 0, 0, 4]);
        assert_eq!(server.get(&server.battery_service.level).unwrap(), 42);
    });
}
