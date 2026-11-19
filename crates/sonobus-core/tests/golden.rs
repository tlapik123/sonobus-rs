//! Golden bytes produced by JUCE 7.0.8 itself (SonoBus 1.7.2's bundled deps/juce), with
//! `.locki/tmp/vt-ref/main.cpp` (build.sh there; output in golden.txt).

use sonobus_core::layout::{ChannelLayout, ChannelLayouts, MAX_CHANNELS};
use sonobus_core::valuetree::{Error, MAX_DEPTH, ValueTree, Var};

const ALL_TYPES: &str = concat!(
    "526f6f7400010e69000105012a0000006e656700010501f9ffffff69363400010906efcdab8967452301740001010266",
    "0001010364000109040000000000000a40666c00010904000000a06e12e3bf730001080568c3a96c6c6f006500010205",
    "0062696e00010508000102ff617272000118070104010501010000000103057800010607010101010200760000750001",
    "01096c6f6e6700022e010561626162616261626162616261626162616261626162616261626162616261626162616261",
    "626162616261626162616261626162616261626162616261626162616261626162616261626162616261626162616261",
    "626162616261626162616261626162616261626162616261626162616261626162616261626162616261626162616261",
    "626162616261626162616261626162616261626162616261626162616261626162616261626162616261626162616261",
    "626162616261626162616261626162616261626162616261626162616261626162616261626162616261626162616261",
    "626162616261626162616261626162616261626162616261626162616261626162616261626162616261626162616261",
    "626162616261626162616261626162616261626162616200010241000101780001050101000000004200000101430001",
    "0179000103057a0000",
);

const LAYOUTS_STEREO: &str = concat!(
    "4368616e6e656c4c61796f757473000001014c61796f75740001056368616e737461727400010501000000006e756d63",
    "68616e00010501020000006e616d6500010205007370616e3100010904000000000000f0bf7370616e32000109040000",
    "00000000f03f010153746572656f50616e6e6572730000010250616e6e657200010170616e0001090400000000000000",
    "000050616e6e657200010170616e00010904000000000000000000",
);

const LAYOUTS_MULTI: &str = concat!(
    "4368616e6e656c4c61796f757473000001044c61796f75740001056368616e737461727400010501000000006e756d63",
    "68616e00010501010000006e616d6500010705566f63616c007370616e3100010904000000000000f0bf7370616e3200",
    "010904000000000000f03f010153746572656f50616e6e6572730000010150616e6e657200010170616e000109040000",
    "00000000d0bf004c61796f75740001056368616e737461727400010501010000006e756d6368616e0001050102000000",
    "6e616d65000106054b657973007370616e3100010904000000000000e8bf7370616e3200010904000000000000e03f01",
    "0153746572656f50616e6e6572730000010250616e6e657200010170616e0001090400000000000000000050616e6e65",
    "7200010170616e000109040000000000000000004c61796f75740001056368616e737461727400010501030000006e75",
    "6d6368616e00010501010000006e616d6500010b054d6574726f6e6f6d65007370616e3100010904000000000000f0bf",
    "7370616e3200010904000000000000f03f010153746572656f50616e6e6572730000010150616e6e657200010170616e",
    "000109040000000000000000004c61796f75740001056368616e737461727400010501040000006e756d6368616e0001",
    "0501020000006e616d6500010f0546696c6520506c61796261636b007370616e3100010904000000000000f0bf737061",
    "6e3200010904000000000000f03f010153746572656f50616e6e6572730000010250616e6e657200010170616e000109",
    "0400000000000000000050616e6e657200010170616e00010904000000000000000000",
);

fn hex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn bytes(t: &ValueTree) -> Vec<u8> {
    let mut out = Vec::new();
    t.write(&mut out);
    out
}

fn tree(type_name: &str, props: Vec<(&str, Var)>, children: Vec<ValueTree>) -> ValueTree {
    ValueTree {
        type_name: type_name.into(),
        properties: props.into_iter().map(|(n, v)| (n.into(), v)).collect(),
        children,
    }
}

#[test]
fn invalid_tree() {
    assert_eq!(bytes(&ValueTree::default()), [0, 0, 0]);
    assert_eq!(ValueTree::read(&[0, 0, 0]), Ok(ValueTree::default()));
}

#[test]
fn all_var_types_match_juce() {
    let golden = hex(ALL_TYPES);
    let expected = tree(
        "Root",
        vec![
            ("i", Var::Int(42)),
            ("neg", Var::Int(-7)),
            ("i64", Var::Int64(0x0123_4567_89ab_cdef)),
            ("t", Var::Bool(true)),
            ("f", Var::Bool(false)),
            ("d", Var::Double(3.25)),
            ("fl", Var::Double((-0.596f32).into())),
            ("s", Var::String("héllo".into())),
            ("e", Var::String(String::new())),
            ("bin", Var::Binary(vec![0, 1, 2, 0xff])),
            (
                "arr",
                Var::Array(vec![
                    Var::Int(1),
                    Var::String("x".into()),
                    Var::Array(vec![Var::Bool(true)]),
                    Var::Void,
                ]),
            ),
            ("v", Var::Void),
            ("u", Var::Undefined),
            ("long", Var::String("ab".repeat(150))),
        ],
        vec![
            tree("A", vec![("x", Var::Int(1))], vec![]),
            tree(
                "B",
                vec![],
                vec![tree("C", vec![("y", Var::String("z".into()))], vec![])],
            ),
        ],
    );
    assert_eq!(ValueTree::read(&golden).unwrap(), expected);
    assert_eq!(bytes(&expected), golden);
}

#[test]
fn stereo_send_layout_matches_juce() {
    let golden = hex(LAYOUTS_STEREO);
    let layouts = ChannelLayouts {
        groups: vec![ChannelLayout {
            num_channels: 2,
            ..Default::default()
        }],
    };
    assert_eq!(bytes(&layouts.to_tree()), golden);
    let back = ChannelLayouts::from_tree(&ValueTree::read(&golden).unwrap()).unwrap();
    assert_eq!(back, layouts);
}

#[test]
fn multichannel_layout_matches_juce() {
    let golden = hex(LAYOUTS_MULTI);
    let group = |chan_start, num_channels, name: &str| ChannelLayout {
        chan_start,
        num_channels,
        name: name.into(),
        ..Default::default()
    };
    let mut vocal = group(0, 1, "Vocal");
    vocal.pan[0] = -0.25;
    let mut keys = group(1, 2, "Keys");
    keys.pan_stereo = [-0.75, 0.5];
    let layouts = ChannelLayouts {
        groups: vec![
            vocal,
            keys,
            group(3, 1, "Metronome"),
            group(4, 2, "File Playback"),
        ],
    };
    assert_eq!(bytes(&layouts.to_tree()), golden);
    let t = ValueTree::read(&golden).unwrap();
    assert_eq!(t.type_name, "ChannelLayouts");
    assert_eq!(t.children[1].get("name"), Some(&Var::String("Keys".into())));
    assert_eq!(ChannelLayouts::from_tree(&t).unwrap(), layouts);
}

#[test]
fn layout_tolerates_missing_and_unknown() {
    let mut current = ChannelLayout {
        num_channels: 3,
        name: "Old".into(),
        ..Default::default()
    };
    current.pan[2] = 0.5;
    let t = tree(
        "Whatever",
        vec![
            ("numchan", Var::Int(1)),
            ("future", Var::Binary(vec![9])),
            ("span1", Var::Int(0)),
        ],
        vec![
            tree("Unknown", vec![], vec![]),
            tree(
                "StereoPanners",
                vec![],
                vec![tree("Panner", vec![("pan", Var::Double(0.25))], vec![])],
            ),
        ],
    );
    current.update_from_tree(&t);
    assert_eq!((current.num_channels, current.name.as_str()), (1, "Old"));
    assert_eq!(current.pan_stereo, [0.0, 1.0]);
    assert_eq!(current.pan[..3], [0.25, 0.0, 0.5]);

    let mut many = ValueTree::new("x");
    many.children = vec![ValueTree::new("Layout"); 70];
    assert_eq!(ChannelLayouts::from_tree(&many).unwrap().groups.len(), 64);
    assert_eq!(ChannelLayouts::from_tree(&ValueTree::default()), None);
    // 64 panners max on the wire
    let wide = ChannelLayout {
        num_channels: 100,
        ..Default::default()
    }
    .to_tree();
    assert_eq!(wide.children[0].children.len(), MAX_CHANNELS);
}

#[test]
fn every_truncation_errors() {
    for golden in [ALL_TYPES, LAYOUTS_STEREO, LAYOUTS_MULTI].map(hex) {
        for n in 0..golden.len() {
            assert!(
                ValueTree::read(&golden[..n]).is_err(),
                "prefix {n} of {}",
                golden.len()
            );
        }
    }
}

#[test]
fn hostile_inputs_error_without_panic() {
    // Trees nested past MAX_DEPTH: "a\0" + 0 props + 1 child, repeated.
    let deep: Vec<u8> = b"a\0\x00\x01\x01".repeat(MAX_DEPTH + 10);
    assert_eq!(ValueTree::read(&deep), Err(Error::TooDeep));
    // Arrays nested past MAX_DEPTH inside one property.
    let mut arr = b"r\0\x01\x01p\0".to_vec();
    arr.extend(b"\x01\x05\x07\x01\x01".repeat(MAX_DEPTH + 10));
    assert_eq!(ValueTree::read(&arr), Err(Error::TooDeep));
    // Huge declared lengths/counts with no data behind them.
    for blob in [
        &b"r\0\x01\x01p\0\x04\xff\xff\xff\x7f\x05abc"[..], // 2 GiB string
        b"r\0\x01\x01p\0\x04\xff\xff\xff\x7f\x08",         // 2 GiB binary
        b"r\0\x01\x01p\0\x01\x02\x07\x04\xff\xff\xff\x7f", // 2^31 array items
        b"r\0\x04\xff\xff\xff\x7f",                        // 2^31 properties
        b"r\0\x00\x04\xff\xff\xff\x7f",                    // 2^31 children
        b"r\0\x09",                                        // bad compressed int
    ] {
        assert!(ValueTree::read(blob).is_err(), "{blob:?}");
    }
}

#[test]
fn juce_tolerances() {
    // Negative property count: JUCE returns the bare tree.
    assert_eq!(ValueTree::read(b"r\0\x81\x01"), Ok(ValueTree::new("r")));
    // Unknown var marker is skipped as void; invalid child ends the child list.
    let t = ValueTree::read(b"r\0\x01\x01p\0\x01\x03\x63ab\x01\x02\0").unwrap();
    assert_eq!(t, tree("r", vec![("p", Var::Void)], vec![]));
    // Duplicate names replace in place.
    let t = ValueTree::read(b"r\0\x01\x03p\0\x01\x01\x02q\0\x01\x01\x03p\0\x01\x01\x03\0").unwrap();
    assert_eq!(
        t.properties,
        vec![
            ("p".into(), Var::Bool(false)),
            ("q".into(), Var::Bool(false))
        ]
    );
}
