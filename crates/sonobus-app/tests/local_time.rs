//! Recording names use local time. Its own test binary: it sets `TZ`, which no other thread
//! may read meanwhile.

#[cfg(unix)]
#[test]
fn local_time_follows_tz() {
    use sonobus_app::controller::local_time;
    let t = 1_700_000_000; // 2023-11-14 22:13:20 UTC
    let hms = |secs| {
        let l = local_time(secs);
        (l.year, l.month, l.day, l.hour, l.minute, l.second)
    };
    // SAFETY: single-threaded test binary.
    unsafe { std::env::set_var("TZ", "UTC") };
    assert_eq!(hms(t), (2023, 11, 14, 22, 13, 20));
    // POSIX TZ: "XYZ-3" is three hours east of UTC
    unsafe { std::env::set_var("TZ", "XYZ-3") };
    assert_eq!(hms(t), (2023, 11, 15, 1, 13, 20));
    unsafe { std::env::set_var("TZ", "XYZ+5:30") };
    assert_eq!(hms(t), (2023, 11, 14, 16, 43, 20));
}
