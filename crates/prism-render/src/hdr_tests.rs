use super::*;

#[test]
fn rgbe_zero_exponent_is_black() {
    let (r, g, b) = rgbe_to_float(255.0, 255.0, 255.0, 0.0);
    assert_eq!((r, g, b), (0.0, 0.0, 0.0));
}

#[test]
fn rgbe_known_value() {
    // E=128 → f = 2^(128-128-8) = 2^-8 = 1/256. R=128 → 128/256 = 0.5.
    let (r, _, _) = rgbe_to_float(128.0, 0.0, 0.0, 128.0);
    assert!((r - 0.5).abs() < 1e-4);
}

#[test]
fn decode_rle_scanline() {
    // Hand-built RLE RGBE file: 1 scanline, 宽度 4.
    // Scanline header: 0x02 0x02 then 宽度 (big-endian) = 4.
    // Each 通道 is a single RLE run of 4 相同 values.
    let header = b"#?RADIANCE\nFORMAT=32-bit_rle_rgbe\n\n-Y 1 +X 4\n";
    let scanline = [
        0x02u8, 0x02, 0x00, 0x04, // RLE marker + width=4 (big-endian)
        0x84, 0x0a, // R: run of 4, value 10
        0x84, 0x14, // G: run of 4, value 20
        0x84, 0x1e, // B: run of 4, value 30
        0x84, 0x80, // E: run of 4, value 128
    ];
    let mut data = header.to_vec();
    data.extend_from_slice(&scanline);

    let (rgba, w, h) = load_rgbe(&data).expect("decode rle");
    assert_eq!((w, h), (4, 1));
    assert_eq!(rgba.len(), 16);
    // E=128 → f = 1/256.
    assert!((rgba[0] - 10.0 / 256.0).abs() < 1e-5);
    assert!((rgba[1] - 20.0 / 256.0).abs() < 1e-5);
    assert!((rgba[2] - 30.0 / 256.0).abs() < 1e-5);
    assert_eq!(rgba[3], 1.0);
}

#[test]
fn decode_rle_literal_run() {
    // Locks the literal-run convention: a count byte <= 128 means exactly
    // `count` literal values (NOT count+1). 宽度 4, all channels literal.
    let header = b"#?RADIANCE\nFORMAT=32-bit_rle_rgbe\n\n-Y 1 +X 4\n";
    let scanline = [
        0x02u8, 0x02, 0x00, 0x04, // RLE marker + width=4 (big-endian)
        // R literal: count=4, values 10,20,30,40
        0x04, 0x0a, 0x14, 0x1e, 0x28, // G literal: count=4, values 1,2,3,4
        0x04, 0x01, 0x02, 0x03, 0x04, // B literal: count=4, values 5,6,7,8
        0x04, 0x05, 0x06, 0x07, 0x08, // E literal: count=4, values 128,128,128,128
        0x04, 0x80, 0x80, 0x80, 0x80,
    ];
    let mut data = header.to_vec();
    data.extend_from_slice(&scanline);

    let (rgba, w, h) = load_rgbe(&data).expect("decode rle literal");
    assert_eq!((w, h), (4, 1));
    // E=128 → f = 1/256.
    assert!((rgba[0] - 10.0 / 256.0).abs() < 1e-5);
    assert!((rgba[1] - 1.0 / 256.0).abs() < 1e-5);
    assert!((rgba[2] - 5.0 / 256.0).abs() < 1e-5);
    assert_eq!(rgba[3], 1.0);
}

#[test]
fn rejects_empty_and_unterminated_header() {
    assert!(load_rgbe(b"").is_err());
    assert!(load_rgbe(b"#?RADIANCE\nFORMAT=32-bit_rle_rgbe\n\n").is_err());
}

#[test]
fn rejects_truncated_rle_scanline() {
    let header = b"-Y 1 +X 4\n";
    // RLE marker + width, then nothing for the four channels.
    let mut data = header.to_vec();
    data.extend_from_slice(&[0x02, 0x02, 0x00, 0x04]);
    assert!(load_rgbe(&data).is_err());
}

#[test]
fn rejects_zero_length_rle_run() {
    let header = b"-Y 1 +X 4\n";
    let mut data = header.to_vec();
    // A zero count would never advance `x`; it must be rejected, not looped.
    data.extend_from_slice(&[0x02, 0x02, 0x00, 0x04, 0x00]);
    assert!(load_rgbe(&data).is_err());
}

#[test]
fn rejects_rle_run_past_scanline() {
    let header = b"-Y 1 +X 4\n";
    let mut data = header.to_vec();
    // R: run of 5 values on a 4-pixel scanline.
    data.extend_from_slice(&[0x02, 0x02, 0x00, 0x04, 0x85, 0x0a]);
    assert!(load_rgbe(&data).is_err());
}

#[test]
fn rejects_truncated_uncompressed_scanline() {
    let header = b"-Y 1 +X 4\n";
    let mut data = header.to_vec();
    data.extend_from_slice(&[1, 2, 3]);
    assert!(load_rgbe(&data).is_err());
}

#[test]
fn rejects_overflowing_dimensions() {
    assert!(load_rgbe(b"-Y 4294967295 +X 4294967295\n").is_err());
}

#[test]
fn decodes_uncompressed_scanline() {
    let header = b"-Y 1 +X 2\n";
    let mut data = header.to_vec();
    data.extend_from_slice(&[128, 64, 32, 128, 0, 0, 0, 0]);
    let (rgba, w, h) = load_rgbe(&data).expect("decode uncompressed");
    assert_eq!((w, h), (2, 1));
    assert!((rgba[0] - 0.5).abs() < 1e-4);
    assert!((rgba[1] - 0.25).abs() < 1e-4);
    assert!((rgba[2] - 0.125).abs() < 1e-4);
    assert_eq!(rgba[3], 1.0);
}

/// RGBE bytes for one pixel with exposure 128 (so the float scale is 1/256).
fn rgbe_pixel(r: u8) -> [u8; 4] {
    [r, 0, 0, 128]
}

#[test]
fn flips_rows_for_positive_y() {
    // `+Y` runs bottom-to-top: the first scanline is the last image row.
    let mut data = b"+Y 2 +X 2\n".to_vec();
    for r in [10u8, 20, 30, 40] {
        data.extend_from_slice(&rgbe_pixel(r));
    }

    let (rgba, w, h) = load_rgbe(&data).expect("decode +Y");
    assert_eq!((w, h), (2, 2));
    // Row 0 (top) comes from the second scanline, row 1 from the first.
    assert!((rgba[0] - 30.0 / 256.0).abs() < 1e-5);
    assert!((rgba[4] - 40.0 / 256.0).abs() < 1e-5);
    assert!((rgba[8] - 10.0 / 256.0).abs() < 1e-5);
    assert!((rgba[12] - 20.0 / 256.0).abs() < 1e-5);
}

#[test]
fn flips_columns_for_negative_x() {
    // `-X` runs right-to-left: the first pixel of the scanline is the last column.
    let mut data = b"-Y 1 -X 2\n".to_vec();
    data.extend_from_slice(&rgbe_pixel(10));
    data.extend_from_slice(&rgbe_pixel(20));

    let (rgba, w, h) = load_rgbe(&data).expect("decode -X");
    assert_eq!((w, h), (2, 1));
    assert!((rgba[0] - 20.0 / 256.0).abs() < 1e-5);
    assert!((rgba[4] - 10.0 / 256.0).abs() < 1e-5);
}

#[test]
fn decodes_transposed_axes() {
    // `+X` first: scanlines are columns, so the file is stored transposed.
    let mut data = b"+X 2 -Y 2\n".to_vec();
    // Column 0 top-down, then column 1 top-down.
    for r in [10u8, 20, 30, 40] {
        data.extend_from_slice(&rgbe_pixel(r));
    }

    let (rgba, w, h) = load_rgbe(&data).expect("decode transposed");
    assert_eq!((w, h), (2, 2));
    // (x, y) order: (0,0), (1,0), (0,1), (1,1) in the output buffer.
    assert!((rgba[0] - 10.0 / 256.0).abs() < 1e-5);
    assert!((rgba[4] - 30.0 / 256.0).abs() < 1e-5);
    assert!((rgba[8] - 20.0 / 256.0).abs() < 1e-5);
    assert!((rgba[12] - 40.0 / 256.0).abs() < 1e-5);
}

#[test]
fn decodes_rle_scanline_with_non_standard_orientation() {
    // Same RLE scanline as `decode_rle_scanline`, but stored bottom-up.
    let mut data = b"+Y 1 +X 4\n".to_vec();
    data.extend_from_slice(&[
        0x02, 0x02, 0x00, 0x04, // RLE marker + width=4
        0x84, 0x0a, 0x84, 0x14, 0x84, 0x1e, 0x84, 0x80,
    ]);

    let (rgba, w, h) = load_rgbe(&data).expect("decode rle +Y");
    assert_eq!((w, h), (4, 1));
    assert!((rgba[0] - 10.0 / 256.0).abs() < 1e-5);
    assert_eq!(rgba[3], 1.0);
}

#[test]
fn rejects_repeated_axis() {
    let mut data = b"-Y 2 -Y 2\n".to_vec();
    data.extend_from_slice(&[0u8; 16]);
    assert!(load_rgbe(&data).is_err());
}

#[test]
fn rejects_zero_dimension() {
    assert!(load_rgbe(b"-Y 0 +X 4\n").is_err());
    assert!(load_rgbe(b"-Y 4 +X 0\n").is_err());
}

#[test]
fn rejects_unparsable_resolution_length() {
    assert!(load_rgbe(b"-Y four +X 4\n").is_err());
    assert!(load_rgbe(b"-Y 4\n").is_err());
}
