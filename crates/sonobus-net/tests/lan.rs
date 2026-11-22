//! LAN discovery over mDNS restricted to IPv4 loopback (nothing leaves the machine), plus the
//! PIN flow glue. Needs multicast on `lo`: when it is unavailable the test says so and skips
//! (the advert codec and browse state machine are unit-tested in `lan.rs` regardless).

mod common;

use std::time::{Duration, Instant};

use common::*;
use sonobus_net::lan::{Advert, AuthKind, DiscoveredSession, Lan, LanBrowser, LanEvent};
use sonobus_net::wire::Role;
use sonobus_net::{Config, Event};

fn found(b: &mut LanBrowser, host: &[u8; 32], within: Duration) -> Option<DiscoveredSession> {
    let end = Instant::now() + within;
    while Instant::now() < end {
        if let Some(LanEvent::Found(s)) = b.recv_timeout(end - Instant::now())
            && s.host.id == *host
        {
            return Some(s);
        }
    }
    None
}

#[test]
#[cfg_attr(
    not(any(target_os = "linux", windows)),
    ignore = "needs distinct loopback addresses"
)]
fn discover_join_with_pin_and_withdraw() {
    let host = simple(Config {
        role: Role::Host,
        session: "Friday jam".into(),
        ..cfg("host", None, None)
    });
    let pin = host.enable_pin();
    assert!(
        pin.len() == 6 && pin.bytes().all(|c| c.is_ascii_digit()),
        "{pin}"
    );
    let hlan = Lan::loopback().unwrap();
    hlan.advertise(
        &host.local_addr(),
        &Advert {
            session: "Friday jam".into(),
            auth: AuthKind::Pin,
        },
    )
    .unwrap();
    let jlan = Lan::loopback().unwrap();
    let mut browser = jlan.browse().unwrap();
    let hid = host.local_addr().id;
    let Some(s) = found(&mut browser, &hid, Duration::from_secs(8)) else {
        println!(
            "SKIPPED: no mDNS over loopback (is multicast enabled on `lo`? \
             `ip link set lo multicast on`); advert/browser logic is covered by unit tests"
        );
        return;
    };
    assert_eq!((s.session.as_str(), s.auth), ("Friday jam", AuthKind::Pin));
    assert!(s.host.addrs.contains(&host.local_addr().addrs[0]), "{s:?}");

    // Join with the PIN; the joiner started under another session name.
    let joiner = simple(Config {
        bind_ip: lo(2).into(),
        ..cfg("joiner", None, None)
    });
    joiner.join_discovered(&s, &format!("{} {}", &pin[..3], &pin[3..]));
    connected(&joiner);
    connected(&host);

    // Completed wrong guesses (from separate addresses, so no back-off) invalidate the PIN...
    for n in 0..auth_pin_failures() {
        let j = simple(Config {
            bind_ip: lo(10 + n as u8).into(),
            ..cfg("guess", None, None)
        });
        let wrong = if pin == "000000" { "111111" } else { "000000" };
        j.join_discovered(&s, wrong);
        assert_eq!(failed(&j).0, "authentication failed");
    }
    wait(&host, |e| matches!(e, Event::PinInvalidated).then_some(()));
    let late = simple(Config {
        bind_ip: lo(30).into(),
        ..cfg("late", None, None)
    });
    late.join_discovered(&s, &pin);
    assert_eq!(failed(&late).0, "authentication failed");
    // ...and a fresh PIN works.
    let pin2 = host.enable_pin();
    late.join_discovered(&s, &pin2);
    connected(&late);

    // Withdrawn: browsers see it go.
    hlan.stop_advertising();
    let end = Instant::now() + Duration::from_secs(8);
    loop {
        assert!(Instant::now() < end, "never saw the session go");
        if browser.recv_timeout(end - Instant::now()) == Some(LanEvent::Lost(hid)) {
            break;
        }
    }
    assert_eq!(browser.sessions().count(), 0);
}

fn auth_pin_failures() -> u32 {
    sonobus_net::auth::PIN_FAILURES
}
