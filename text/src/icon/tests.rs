use super::*;

const STATUS: &str = "/usr/share/icons/Adwaita/symbolic/status";

/// A square drawn by hand, so the em mapping can be checked exactly: a 16 px
/// viewBox with a rect from (2,2) to (14,14) must land at (0.125, 0.125) to
/// (0.875, 0.875), y having been flipped.
#[test]
fn the_viewbox_becomes_the_em_square_with_y_up() {
    let svg =
        br##"<svg viewBox="0 0 16 16" width="16" height="16" xmlns="http://www.w3.org/2000/svg">
        <path d="M 2 2 L 14 2 L 14 14 L 2 14 Z" fill="#000"/>
    </svg>"##;
    let mut builder = Builder::new();
    let icon = load(&mut builder, "hand-drawn-square", svg).expect("a square is an SVG");
    let set = builder.finish();
    assert_eq!(icon.report.filled_paths, 1);
    assert!(icon.report.clean(), "{:?}", icon.report);
    let glyph = set.glyphs[icon.glyph as usize];
    for (got, want) in glyph.bounds.iter().zip([0.125, 0.125, 0.875, 0.875]) {
        assert!((got - want).abs() < 1e-5, "bounds {:?}", glyph.bounds);
    }
    assert!((glyph.advance - 1.0).abs() < 1e-6);
}

#[test]
fn a_stroke_is_ignored_and_counted() {
    let svg =
        br##"<svg viewBox="0 0 16 16" width="16" height="16" xmlns="http://www.w3.org/2000/svg">
        <path d="M 2 8 L 14 8" stroke="#000" stroke-width="2" fill="none"/>
    </svg>"##;
    let mut builder = Builder::new();
    let icon = load(&mut builder, "a-stroke", svg).expect("parses");
    assert_eq!(icon.report.stroked_paths, 1);
    assert_eq!(icon.report.unfilled_paths, 1);
    assert_eq!(icon.report.filled_paths, 0);
    assert!(!icon.report.clean());
    // And it contributed no ink, rather than something wrong.
    let set = builder.finish();
    assert_eq!(set.glyphs[icon.glyph as usize].hband_count, 0);
}

#[test]
fn a_gradient_fill_is_flattened_and_counted() {
    let svg =
        br##"<svg viewBox="0 0 16 16" width="16" height="16" xmlns="http://www.w3.org/2000/svg">
        <defs><linearGradient id="g"><stop offset="0" stop-color="#000"/>
        <stop offset="1" stop-color="#fff"/></linearGradient></defs>
        <path d="M 2 2 L 14 2 L 14 14 Z" fill="url(#g)"/>
    </svg>"##;
    let mut builder = Builder::new();
    let icon = load(&mut builder, "a-gradient", svg).expect("parses");
    assert_eq!(icon.report.non_solid_fills, 1);
    assert_eq!(icon.report.filled_paths, 1);
    // The ink is still there — only the colour was dropped.
    let set = builder.finish();
    assert!(set.glyphs[icon.glyph as usize].hband_count > 0);
}

#[test]
fn an_even_odd_path_sets_the_flag() {
    let svg =
        br##"<svg viewBox="0 0 16 16" width="16" height="16" xmlns="http://www.w3.org/2000/svg">
        <path d="M 1 1 L 15 1 L 15 15 L 1 15 Z M 4 4 L 12 4 L 12 12 L 4 12 Z"
              fill="#000" fill-rule="evenodd"/>
    </svg>"##;
    let mut builder = Builder::new();
    let icon = load(&mut builder, "a-ring", svg).expect("parses");
    let set = builder.finish();
    assert_eq!(
        set.glyphs[icon.glyph as usize].flags & crate::layout::FILL_EVEN_ODD,
        crate::layout::FILL_EVEN_ODD
    );
}

/// The real thing: every Adwaita status icon through the loader. The gate is
/// that each one parses and produces ink, and the tally of what had to be
/// ignored answers "which of them needed anything".
#[test]
fn every_adwaita_status_icon_loads() {
    let dir = std::path::Path::new(STATUS);
    let Ok(entries) = std::fs::read_dir(dir) else {
        eprintln!("skipping: {STATUS} is not installed");
        return;
    };
    let mut files: Vec<std::path::PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "svg"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "{STATUS} holds no SVGs");

    let mut builder = Builder::new();
    let mut dirty: Vec<String> = Vec::new();
    let mut inkless: Vec<String> = Vec::new();
    for file in &files {
        let icon = load_file(&mut builder, file)
            .unwrap_or_else(|err| panic!("{}: {err:#}", file.display()));
        if !icon.report.clean() {
            dirty.push(format!("{} {:?}", icon.name, icon.report));
        }
        if icon.report.curves == 0 {
            inkless.push(icon.name.clone());
        }
    }
    let set = builder.finish();
    assert_eq!(set.glyphs.len(), files.len());
    eprintln!(
        "{} Adwaita status icons: {} bytes of buffers, worst band {} curves",
        files.len(),
        set.bytes(),
        set.worst_band()
    );
    if !dirty.is_empty() {
        eprintln!("icons that needed something ignored ({}):", dirty.len());
        for line in &dirty {
            eprintln!("  {line}");
        }
    }
    assert!(inkless.is_empty(), "icons with no ink at all: {inkless:?}");
}
