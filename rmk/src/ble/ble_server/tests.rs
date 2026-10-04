use core::cell::Cell;
use core::convert::Infallible;
use core::future::Future;
use core::pin::{Pin, pin};
use core::task::{Context, Waker};

use bt_hci::cmd::{self, AsyncCmd, SyncCmd};
use bt_hci::controller::{Controller, ControllerCmdAsync, ControllerCmdSync};
use bt_hci::{ControllerToHostPacket, FromHciBytes};
use trouble_host::att::{Att, AttServer};

use super::*;
use crate::core_traits::Runnable;
use crate::event::{BatteryAdcEvent, ChargingStateEvent, publish_event};
use crate::input_device::battery::BatteryProcessor;

struct ReadController<'a> {
    resume: &'a Cell<bool>,
    next: Cell<usize>,
    encrypted: bool,
}

impl embedded_io_async::ErrorType for ReadController<'_> {
    type Error = Infallible;
}

impl Controller for ReadController<'_> {
    type Buffer<'a> = [u8; 64];

    fn alloc_buf(&self) -> Result<Self::Buffer<'_>, Self::Error> {
        Ok([0; 64])
    }

    async fn write_acl_data(&self, _: &bt_hci::data::AclPacket<'_>) -> Result<(), Self::Error> {
        panic!("unexpected ACL write")
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
            core::future::pending::<()>().await;
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
        panic!("unexpected HCI command")
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

fn with_connection(encrypted: bool, test: impl FnOnce(&Server<'_>, &Connection<'_, DefaultPacketPool>)) {
    let mut resources: trouble_host::HostResources<DefaultPacketPool, 1, 1> = trouble_host::HostResources::new();
    let resume = Cell::new(false);
    let stack = trouble_host::new(
        ReadController {
            next: Cell::new(0),
            encrypted,
            resume: &resume,
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
    let (mut rx, _control, _tx) = stack.runner().split();
    let mut run = pin!(rx.run());
    assert_pending(run.as_mut());
    let conn = stack.peripheral().try_accept().unwrap();
    resume.set(true);
    assert_pending(run.as_mut());
    assert_eq!(conn.security_level().unwrap().encrypted(), encrypted);
    test(&Server::new(AttributeTable::new()), &conn);
}

fn reply(server: &Server<'_>, conn: &Connection<'_, DefaultPacketPool>, request: &[u8]) -> std::vec::Vec<u8> {
    let Att::Client(request) = Att::decode(request).unwrap() else {
        panic!("not an ATT request")
    };
    let mut buffer = [0; 23];
    if let Some(response) = server.handle_read(conn, &request) {
        let response = Att::Server(AttServer::Response(response));
        let mut encoded = std::vec![0; response.size()];
        response.encode(&mut encoded).unwrap();
        encoded
    } else {
        let len = server.process(conn, &request, &mut buffer).unwrap().unwrap();
        buffer[..len].to_vec()
    }
}

#[test]
fn battery_att_replies_distinguish_unmeasured_zero_and_history() {
    with_connection(true, |server, conn| {
        let handle = server.battery_service.level.handle;
        let [lo, hi] = handle.to_le_bytes();
        assert_eq!(reply(server, conn, &[0x0a, lo, hi]), [1, 0x0a, lo, hi, 0x0e]);
        assert_eq!(reply(server, conn, &[0x0c, lo, hi, 0, 0]), [1, 0x0c, lo, hi, 0x0e]);

        let mut battery = BatteryProcessor::new(1, 1);
        let mut run = pin!(battery.run());
        assert_pending(run.as_mut());
        for (sample, level) in [(4055, 0), (4566, 73)] {
            publish_event(BatteryAdcEvent(sample));
            assert_pending(run.as_mut());
            assert_eq!(reply(server, conn, &[0x0a, lo, hi]), [0x0b, level]);
            assert_eq!(reply(server, conn, &[0x0c, lo, hi, 0, 0]), [0x0d, level]);
        }
        for charging in [true, false] {
            publish_event(ChargingStateEvent { charging });
            assert_pending(run.as_mut());
        }
        assert_eq!(reply(server, conn, &[0x0a, lo, hi]), [0x0b, 73]);
        assert_eq!(reply(server, conn, &[0x0c, lo, hi, 1, 0]), [0x0d]);
        assert_eq!(reply(server, conn, &[0x0c, lo, hi, 2, 0]), [1, 0x0c, lo, hi, 7]);
        server.set(&server.battery_service.level, &0).unwrap();
        assert_eq!(reply(server, conn, &[0x0a, lo, hi]), [0x0b, 73]);
    });
}

#[test]
fn battery_reads_keep_encryption_permissions() {
    with_connection(false, |server, conn| {
        let [lo, hi] = server.battery_service.level.handle.to_le_bytes();
        assert_eq!(reply(server, conn, &[0x0a, lo, hi]), [1, 0x0a, lo, hi, 5]);
    });
}

#[test]
fn unrelated_reads_and_discovery_leave_battery_attributes_untouched() {
    with_connection(true, |server, conn| {
        let mut battery = BatteryProcessor::new(1, 1);
        let mut run = pin!(battery.run());
        assert_pending(run.as_mut());
        publish_event(BatteryAdcEvent(4566));
        assert_pending(run.as_mut());
        server.set(&server.battery_service.level, &42).unwrap();
        let [lo, hi] = server.device_config_service.manufacturer_name.handle.to_le_bytes();
        for request in [
            std::vec![0x0a, lo, hi],
            std::vec![8, 1, 0, 0xff, 0xff, 3, 0x28],
            std::vec![8, 1, 0, 0xff, 0xff, 0x19, 0x2a],
            std::vec![0x10, 1, 0, 0xff, 0xff, 0, 0x28],
        ] {
            let Att::Client(decoded) = Att::decode(&request).unwrap() else {
                unreachable!()
            };
            assert!(server.handle_read(conn, &decoded).is_none());
            let mut expected = [0; 23];
            let len = server.process(conn, &decoded, &mut expected).unwrap().unwrap();
            assert_eq!(reply(server, conn, &request), expected[..len]);
            assert_eq!(server.get(&server.battery_service.level).unwrap(), 42);
        }
    });
}

#[cfg(feature = "split")]
#[test]
fn peripheral_reads_use_their_own_history() {
    with_connection(true, |server, conn| {
        let Some((peripheral, &id)) = server
            .peripheral_battery_services
            .levels
            .first()
            .zip(crate::SPLIT_BATTERY_PERIPHERAL_IDS.first())
        else {
            return;
        };
        let central = server.battery_service.level;
        let [lo, hi] = peripheral.handle.to_le_bytes();
        assert_eq!(reply(server, conn, &[0x0a, lo, hi]), [1, 0x0a, lo, hi, 0x0e]);
        let mut battery = BatteryProcessor::new(1, 1);
        let mut run = pin!(battery.run());
        assert_pending(run.as_mut());
        publish_event(BatteryAdcEvent(4405));
        assert_pending(run.as_mut());
        let [cl, ch] = central.handle.to_le_bytes();
        assert_eq!(server.get(peripheral).unwrap(), 0);

        for level in [Some(0), Some(81), None] {
            crate::split::driver::set_peripheral_battery(
                id,
                rmk_types::battery::BatteryStatus::Available {
                    charge_state: rmk_types::battery::ChargeState::Discharging,
                    level,
                },
            );
            let expected = level.unwrap_or(81);
            server.set(&central, &42).unwrap();
            assert_eq!(reply(server, conn, &[0x0a, lo, hi]), [0x0b, expected]);
            assert_eq!(server.get(&central).unwrap(), 42);
            assert_eq!(reply(server, conn, &[0x0c, lo, hi, 0, 0]), [0x0d, expected]);
        }
        assert_eq!(reply(server, conn, &[0x20, cl, ch, lo, hi]), [1, 0x20, 0, 0, 6]);
    });
}

#[test]
fn read_multiple_reports_the_unknown_handle_or_unsupported_request() {
    with_connection(true, |server, conn| {
        let [lo, hi] = server.battery_service.level.handle.to_le_bytes();
        let [dl, dh] = server.device_config_service.manufacturer_name.handle.to_le_bytes();
        server.set(&server.battery_service.level, &42).unwrap();
        assert_eq!(reply(server, conn, &[0x20, dl, dh, lo, hi]), [1, 0x20, lo, hi, 0x0e]);
        assert_eq!(server.get(&server.battery_service.level).unwrap(), 42);
        let mut battery = BatteryProcessor::new(1, 1);
        let mut run = pin!(battery.run());
        assert_pending(run.as_mut());
        publish_event(BatteryAdcEvent(4055));
        assert_pending(run.as_mut());
        assert_eq!(reply(server, conn, &[0x20, dl, dh, lo, hi]), [1, 0x20, 0, 0, 6]);
        assert_eq!(server.get(&server.battery_service.level).unwrap(), 42);
        assert_eq!(reply(server, conn, &[0x20, dl, dh, 0, 0]), [1, 0x20, 0, 0, 1]);
        assert_eq!(reply(server, conn, &[0x20, lo, hi]), [1, 0x20, 0, 0, 4]);
    });
}
