use scene_meme::{AudioHop, Brief, FrameMetrics, Perceive, pick_peaks};
fn fixture() -> Perceive {
    Perceive {
        fps: 30.0,
        duration_s: 3.0,
        has_audio: false,
        encoder: "dhash".into(),
        frames: (0..90)
            .map(|i| FrameMetrics {
                frame: i,
                t: i as f64 / 30.0,
                sharpness: 0.8,
                sharp_raw: 0.8,
                motion: 0.5,
                brightness: 0.5,
                contrast: 0.5,
                change: 0.01,
                dhash: i.wrapping_mul(0x9e3779b97f4a7c15),
                tags: vec![],
            })
            .collect(),
        audio: (0..90)
            .map(|i| AudioHop::silent(i, i as f64 / 30.0))
            .collect(),
    }
}
#[test]
fn dense_cuts_use_exclusive_search_and_keep_representative_time() {
    let mut p = fixture();
    p.frames[30].change = 0.8;
    p.frames[32].change = 0.9;
    p.frames[30].sharpness = 0.1;
    p.frames[31].sharpness = 0.9;
    p.frames[32].sharpness = 0.95;
    let result = pick_peaks(&p, None, &Brief::default(), &[]);
    assert_eq!(
        result.iter().map(|c| c.frame).collect::<Vec<_>>(),
        vec![31, 32]
    );
    assert_eq!(result[0].t, 1.0);
    assert_eq!(result[0].representative_t, 31.0 / 30.0);
}
#[test]
fn raw_cosine_and_quality_order_control_dedup() {
    let mut p = fixture();
    p.frames[20].change = 0.36;
    p.frames[50].change = 0.99;
    let mut emb = vec![vec![0.0, 1.0]; 90];
    emb[20] = vec![1.0, 0.0];
    emb[50] = vec![0.9, 0.4358899];
    assert_eq!(pick_peaks(&p, Some(&emb), &Brief::default(), &[]).len(), 2);
    emb[50] = emb[20].clone();
    let result = pick_peaks(&p, Some(&emb), &Brief::default(), &[]);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].frame, 50);
}
#[test]
fn config_aliases_conflicts_unknowns_and_nonfinite_values() {
    let b: Brief =
        serde_json::from_str(r#"{"brightness_lo":0.1,"brightness_hi":0.9,"snap_to_sharp":false}"#)
            .unwrap();
    assert_eq!(b.brightness_min, 0.1);
    assert!(!b.snap_to_sharp);
    for text in [
        r#"{"brightness_lo":0.1,"brightness_min":0.2}"#,
        r#"{"typo":true}"#,
    ] {
        assert!(serde_json::from_str::<Brief>(text).is_err());
    }
    let b = Brief {
        gemini_window_sec: f64::NAN,
        ..Brief::default()
    };
    assert!(b.validate().is_err());
}
#[test]
fn retry_preserves_other_beats_and_global_analysis() {
    use scene_meme::gemini::GeminiAnalysis;
    let mut original: GeminiAnalysis =
        serde_json::from_value(serde_json::json!({"joke":"original", "beats":[
        {"id":"f1","t":1,"note":"old"},{"id":"f2","t":2,"note":"keep me"}]}))
        .unwrap();
    let update =
        serde_json::from_value(serde_json::json!({"beats":[{"id":"f1","t":1,"note":"new"}]}))
            .unwrap();
    original.merge_window("f1", update);
    assert_eq!(original.beats.len(), 2);
    assert_eq!(original.beats[0].note, "new");
    assert_eq!(original.beats[1].note, "keep me");
    assert_eq!(original.joke, "original");
}
#[test]
fn embedding_cache_shape_and_values_are_checked() {
    use scene_meme::embed::EmbedOut;
    let good = EmbedOut {
        embeddings: vec![vec![1.0, 0.0]],
        vocab_embeddings: vec![vec![0.0, 1.0]],
    };
    assert!(good.validate(1, 1).is_ok());
    assert!(good.validate(2, 1).is_err());
    assert!(good.validate(1, 2).is_err());
    for row in [vec![1.0], vec![f32::NAN, 0.0], vec![0.0, 0.0]] {
        let bad = EmbedOut {
            embeddings: vec![row],
            vocab_embeddings: good.vocab_embeddings.clone(),
        };
        assert!(bad.validate(1, 1).is_err());
    }
}
