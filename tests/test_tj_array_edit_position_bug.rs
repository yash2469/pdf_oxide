//! Regression tests for text-editing collateral damage on source-loaded pages.
//!
//! Editing pre-existing text via `PdfPage::set_text` routes through the
//! *destructive redaction* engine (see `DocumentEditor::save_page` →
//! `apply_redactions_destructive_for_page`, the #940 fix). That engine is
//! tuned for redaction, where over-removal is the safe direction and
//! inter-glyph spacing is deliberately dropped so no positional delta can
//! leak a removed glyph (`text_prune::prune_run`, G2). Both properties are
//! wrong for an *edit*, whose contract is "change this span, leave the rest
//! of the page byte-for-byte equivalent":
//!
//!   1. Glyph boxes are built from font ascent/descent (`text_engine::
//!      glyph_box`, default 1.0/-0.30 em → 1.3 em tall), so a 12pt line's
//!      glyphs reach ~3.6pt above the ascent of a 12pt-leaded neighbour.
//!      The edited span's erasure rect therefore intersects — and deletes —
//!      glyphs belonging to an adjacent, untouched line.
//!   2. Every surviving run is re-anchored absolutely and its original
//!      inter-glyph advances discarded, so untouched text loses the
//!      TJ-array kerning that encoded its word spaces.
//!
//! Both are observable through `extract_chars()` / `extract_text()` on the
//! saved file. Neither is TJ-array-specific: case (1) reproduces on a page
//! built purely from plain `Tj` operators.

use pdf_oxide::document::PdfDocument;
use pdf_oxide::editor::DocumentEditor;

fn write_temp(bytes: &[u8], name: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(name);
    std::fs::write(&path, bytes).unwrap();
    path
}

fn finalize_pdf(pdf: &mut Vec<u8>, obj_offsets: &[usize]) {
    let xref_offset = pdf.len();
    let count = obj_offsets.len();
    pdf.extend_from_slice(format!("xref\n0 {}\n", count).as_bytes());
    pdf.extend_from_slice(b"0000000000 65535 f \r\n");
    for &off in &obj_offsets[1..] {
        pdf.extend_from_slice(format!("{:010} 00000 n \r\n", off).as_bytes());
    }
    let trailer = format!(
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
        count, xref_offset
    );
    pdf.extend_from_slice(trailer.as_bytes());
}

fn build_pdf(content: &str) -> Vec<u8> {
    let mut pdf = b"%PDF-1.7\n".to_vec();
    let off1 = pdf.len();
    pdf.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");
    let off2 = pdf.len();
    pdf.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n");
    let off3 = pdf.len();
    pdf.extend_from_slice(
        b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>\nendobj\n",
    );
    let off4 = pdf.len();
    pdf.extend_from_slice(format!("4 0 obj\n<< /Length {} >>\nstream\n", content.len()).as_bytes());
    pdf.extend_from_slice(content.as_bytes());
    pdf.extend_from_slice(b"\nendstream\nendobj\n");
    let off5 = pdf.len();
    pdf.extend_from_slice(
        b"5 0 obj\n<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>\nendobj\n",
    );
    finalize_pdf(&mut pdf, &[0, off1, off2, off3, off4, off5]);
    pdf
}

/// Two 12pt lines at 12pt leading, drawn with plain `Tj` — no TJ array.
const PLAIN_TWO_LINES: &str = concat!(
    "BT\n/F1 12 Tf\n",
    "1 0 0 1 28 132 Tm\n(UPPER LINE ALPHA BRAVO CHARLIE) Tj\n",
    "1 0 0 1 28 120 Tm\n(lower line target) Tj\n",
    "ET"
);

/// The real-resume shape: one kerned TJ array holding the degree line and a
/// right-aligned "CGPA: 8.97", plus a second line below.
const KERNED_TJ_ARRAY: &str = concat!(
    "BT\n/F1 10.9091 Tf\n28 132 Td\n",
    "[(Bachelor)-333(of)-333(Technology)-333(in)-333(Information)-333(Technology)",
    "-22733(CGP)96(A:)-384(8.97)]TJ\n",
    "0 -12 Td\n(Janakpuri, New Delhi) Tj\n",
    "ET"
);

/// Edit the single span containing `needle` to `replacement` and return the
/// saved bytes.
fn edit_span(pdf: Vec<u8>, needle: &str, replacement: &str) -> Vec<u8> {
    let mut editor = DocumentEditor::from_bytes(pdf).unwrap();
    let mut page = editor.get_page(0).unwrap();
    assert!(page.is_loaded_from_source());
    let matches = page.find_text_containing(needle);
    assert!(!matches.is_empty(), "no span containing {needle:?}");
    let id = matches[0].id();
    page.set_text(id, replacement).unwrap();
    editor.save_page(page).unwrap();
    editor.save_to_bytes().unwrap()
}

fn extract_text_of(bytes: &[u8], name: &str) -> String {
    let path = write_temp(bytes, name);
    let doc = PdfDocument::open(&path).unwrap();
    let text = doc.extract_text(0).unwrap();
    let _ = std::fs::remove_file(&path);
    text
}

/// Non-whitespace glyphs with their x positions, for rows above `y_min`.
fn glyphs_above(bytes: &[u8], name: &str, y_min: f32) -> Vec<(char, f32)> {
    let path = write_temp(bytes, name);
    let doc = PdfDocument::open(&path).unwrap();
    let chars = doc.extract_chars(0).unwrap();
    let _ = std::fs::remove_file(&path);
    let mut v: Vec<(char, f32)> = chars
        .iter()
        .filter(|c| c.bbox.y > y_min && !c.char.is_whitespace())
        .map(|c| (c.char, c.bbox.x))
        .collect();
    v.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
    v
}

/// Defect 1 — the collateral-deletion bug, on plain `Tj` content.
///
/// Editing the *lower* line must not disturb the line above it. Pre-fix the
/// upper line comes back as "PHA BRAVO CHARLIE": the leading 13 glyphs
/// ("UPPER LINE AL", i.e. everything within the erasure rect's x-range) are
/// deleted because their ascent-derived boxes dip into the rect.
#[test]
fn editing_a_line_does_not_delete_glyphs_from_the_line_above() {
    let out = edit_span(build_pdf(PLAIN_TWO_LINES), "lower line target", "EDITED");
    let text = extract_text_of(&out, "tj_bug_plain_neighbour.pdf");
    assert!(
        text.contains("UPPER LINE ALPHA BRAVO CHARLIE"),
        "the untouched line above the edit lost glyphs; got {text:?}"
    );
    assert!(text.contains("EDITED"), "the edit itself must be applied; got {text:?}");
}

/// Defect 1, again, on the real kerned-TJ-array shape: editing the fragment
/// inside the array must not disturb the separate line below it.
#[test]
fn editing_a_tj_array_fragment_does_not_damage_the_line_below() {
    let out = edit_span(build_pdf(KERNED_TJ_ARRAY), "8.97", "CGPA: 9.50");
    let text = extract_text_of(&out, "tj_bug_array_neighbour.pdf");
    assert!(
        text.contains("Janakpuri, New Delhi"),
        "the untouched line below the edit was damaged; got {text:?}"
    );
    assert!(text.contains("9.50"), "the edit itself must be applied; got {text:?}");
}

/// Defect 2 — spacing fidelity of untouched text.
///
/// The word spaces of the sibling fragments in the edited TJ array were
/// encoded as array adjustments. `prune_run` re-anchors each surviving run
/// and drops those advances, so pre-fix the untouched degree line reads
/// "BachelorofTechnologyinInformationTechnology".
#[test]
fn editing_a_tj_array_fragment_preserves_word_spacing_of_untouched_text() {
    let out = edit_span(build_pdf(KERNED_TJ_ARRAY), "8.97", "CGPA: 9.50");
    let text = extract_text_of(&out, "tj_bug_array_spacing.pdf");
    assert!(
        text.contains("Bachelor of Technology in Information Technology"),
        "untouched text in the edited TJ array lost its word spacing; got {text:?}"
    );
}

/// Defect 2's geometric counterpart: untouched glyphs must not move.
///
/// This is the check that distinguishes a real fix from one that merely
/// makes `extract_text` look right — it compares absolute glyph x positions
/// before and after the edit.
#[test]
fn untouched_glyph_positions_are_unchanged_by_an_edit() {
    let pdf = build_pdf(KERNED_TJ_ARRAY);
    let before = glyphs_above(&pdf, "tj_bug_geo_before.pdf", 130.0);
    let out = edit_span(pdf, "8.97", "CGPA: 9.50");
    let after = glyphs_above(&out, "tj_bug_geo_after.pdf", 130.0);

    // Restrict to the untouched prefix (left of the CGPA fragment at x≈522).
    let keep = |v: &Vec<(char, f32)>| -> Vec<(char, f32)> {
        v.iter().filter(|(_, x)| *x < 300.0).cloned().collect()
    };
    let b = keep(&before);
    let a = keep(&after);
    assert_eq!(
        b.iter().map(|v| v.0).collect::<String>(),
        a.iter().map(|v| v.0).collect::<String>(),
        "the untouched glyph sequence changed"
    );
    for (i, ((cb, xb), (_, xa))) in b.iter().zip(a.iter()).enumerate() {
        assert!(
            (xb - xa).abs() < 0.05,
            "untouched glyph {i} ({cb:?}) moved from x={xb:.3} to x={xa:.3}"
        );
    }
}

/// Guard the erasure itself still works: the replaced text must be gone.
#[test]
fn the_replaced_text_is_actually_removed() {
    let out = edit_span(build_pdf(KERNED_TJ_ARRAY), "8.97", "CGPA: 9.50");
    let text = extract_text_of(&out, "tj_bug_erased.pdf");
    assert!(!text.contains("8.97"), "old value must not survive; got {text:?}");
}

/// Two *separate* fragments of the same TJ array edited in one save, with
/// untouched fragments both between and around them. Exercises the
/// multi-region path through `rebuild_tj_preserving_offsets`: several
/// erasure rects apply to one array, and the surviving fragments must keep
/// both their glyphs and their spacing.
#[test]
fn two_edits_in_one_tj_array_leave_the_other_fragments_intact() {
    let mut editor = DocumentEditor::from_bytes(build_pdf(KERNED_TJ_ARRAY)).unwrap();
    let mut page = editor.get_page(0).unwrap();

    let first = page.find_text_containing("8.97")[0].id();
    page.set_text(first, "CGPA: 9.50").unwrap();
    let second = page.find_text_containing("Information Technology")[0].id();
    page.set_text(second, "Computer Science ").unwrap();

    editor.save_page(page).unwrap();
    let out = editor.save_to_bytes().unwrap();
    let text = extract_text_of(&out, "tj_bug_two_edits.pdf");

    assert!(text.contains("9.50"), "first edit missing; got {text:?}");
    assert!(text.contains("Computer Science"), "second edit missing; got {text:?}");
    assert!(!text.contains("8.97"), "first old value survived; got {text:?}");
    assert!(
        text.contains("Bachelor of Technology in"),
        "the fragment between/before the two edits lost glyphs or spacing; got {text:?}"
    );
    assert!(
        text.contains("Janakpuri, New Delhi"),
        "the untouched line below was damaged; got {text:?}"
    );
}
