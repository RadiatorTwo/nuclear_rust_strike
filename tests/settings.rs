use nstrike::settings::{parse, render, FrameRate, Settings};

#[test]
fn written_file_reads_back() {
    let all = [
        Settings::default(),
        Settings { window: Some((1920, 1080)), fullscreen: true, frame_rate: FrameRate::Fps(144), msaa: 8, texture_filter: 0 },
        Settings { frame_rate: FrameRate::Unlimited, ..Settings::default() },
    ];
    for s in all {
        let mut warnings = Vec::new();
        assert_eq!(parse(&render(&s), |w| warnings.push(w)), s);
        assert!(warnings.is_empty(), "{warnings:?}");
    }
}

#[test]
fn wrong_values_keep_defaults() {
    let mut warnings = Vec::new();
    let s = parse("msaa = 3\nwindow = big\nspeed = 2\nnonsense\nfullscreen = true # own comment\n", |w| warnings.push(w));
    assert_eq!(s, Settings { fullscreen: true, ..Settings::default() });
    assert_eq!(warnings.len(), 4, "{warnings:?}");
}
